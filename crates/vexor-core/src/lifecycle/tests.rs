use super::*;
use crate::restart::{Backoff, MIN_RESTART_INTERVAL};
use proptest::prelude::*;

const MS: fn(u64) -> Duration = Duration::from_millis;

fn ctx(now: u64) -> Ctx {
    Ctx {
        now: Millis(now),
        rand: 0,
    }
}

fn exit(code: i32) -> Event {
    Event::Exited(ExitStatus {
        code: Some(code),
        signal: None,
    })
}

fn armed(fx: &[Effect]) -> Option<(Timer, Duration, u64)> {
    fx.iter().find_map(|e| match *e {
        Effect::ArmTimer {
            timer,
            after,
            generation,
        } => Some((timer, after, generation)),
        _ => None,
    })
}

fn fire(i: &mut Instance, now: u64) -> Vec<Effect> {
    let (timer, generation) = i.armed_timer().expect("a timer is armed");
    i.handle(Event::TimerFired { timer, generation }, ctx(now))
}

/// Invariants that must hold after every event, in every state.
fn check_invariants(i: &Instance) {
    let timer = i.armed_timer().map(|(t, _)| t);
    match i.state() {
        State::Backoff => assert_eq!(timer, Some(Timer::Backoff)),
        State::Stopping => assert!(matches!(timer, None | Some(Timer::Stop))),
        State::Starting => {
            let expect = i.policy().ready_timeout.map(|_| Timer::Ready);
            assert_eq!(timer, expect);
        }
        State::Online | State::Unhealthy => {
            // Only a reload arms a timer while running.
            let expect = i.reloading().then_some(Timer::Replacement);
            assert_eq!(timer, expect, "{:?} reloading={}", i.state(), i.reloading());
        }
        State::Stopped | State::Errored | State::Unknown => {
            assert_eq!(timer, None, "no timer in {:?}", i.state());
        }
    }
    if i.reloading() {
        assert!(matches!(i.state(), State::Online | State::Unhealthy));
        assert!(
            i.policy().reload.is_some(),
            "reload without a start-first policy"
        );
    }
    assert_eq!(i.state() == State::Errored, i.error().is_some());
}

fn check_effects(i: &Instance, fx: &[Effect]) {
    for e in fx {
        match *e {
            Effect::Spawn => assert_eq!(i.state(), State::Starting),
            Effect::SignalStop => assert_eq!(i.state(), State::Stopping),
            Effect::ForceKill => assert_eq!(i.state(), State::Stopping),
            Effect::ArmTimer {
                timer, generation, ..
            } => assert_eq!(i.armed_timer(), Some((timer, generation))),
            Effect::SpawnReplacement => assert!(i.reloading()),
            Effect::PromoteReplacement => {
                assert!(!i.reloading());
                assert_eq!(i.state(), State::Online);
            }
            Effect::AbandonReplacement => assert!(!i.reloading()),
        }
    }
}

fn step(i: &mut Instance, e: Event, now: u64) -> Vec<Effect> {
    let fx = i.handle(e, ctx(now));
    check_invariants(i);
    check_effects(i, &fx);
    fx
}

fn online(policy: InstancePolicy) -> Instance {
    let mut i = Instance::new(policy);
    step(&mut i, Event::Start, 0);
    step(&mut i, Event::Spawned, 0);
    i
}

// ---------------------------------------------------------------- basic flow

#[test]
fn start_spawns_and_goes_online() {
    let mut i = Instance::new(InstancePolicy::default());
    assert_eq!(step(&mut i, Event::Start, 0), vec![Effect::Spawn]);
    assert_eq!(i.state(), State::Starting);
    assert_eq!(step(&mut i, Event::Spawned, 5), vec![]);
    assert_eq!(i.state(), State::Online);
    assert_eq!(i.desired(), Desired::Running);
}

#[test]
fn start_is_idempotent_while_running() {
    let mut i = online(InstancePolicy::default());
    assert_eq!(step(&mut i, Event::Start, 10), vec![]);
    assert_eq!(i.state(), State::Online);
    assert_eq!(i.restarts(), 0);
}

#[test]
fn graceful_stop_then_force_kill_at_timeout() {
    let mut i = online(InstancePolicy::default());
    let fx = step(&mut i, Event::Stop, 100);
    assert_eq!(fx[0], Effect::SignalStop);
    assert_eq!(
        armed(&fx).map(|a| (a.0, a.1)),
        Some((Timer::Stop, MS(1600)))
    );
    assert_eq!(fire(&mut i, 1700), vec![Effect::ForceKill]);
    assert_eq!(i.state(), State::Stopping);
    step(&mut i, exit(137), 1710);
    assert_eq!(i.state(), State::Stopped);
    assert_eq!(i.desired(), Desired::Stopped);
}

#[test]
fn exit_during_graceful_stop_cancels_force_kill() {
    let mut i = online(InstancePolicy::default());
    step(&mut i, Event::Stop, 100);
    let (timer, generation) = i.armed_timer().expect("stop timer");
    step(&mut i, exit(0), 200);
    assert_eq!(i.state(), State::Stopped);
    // The stop timer firing late must not kill anything.
    assert_eq!(
        step(&mut i, Event::TimerFired { timer, generation }, 1700),
        vec![]
    );
}

#[test]
fn restart_stops_then_respawns() {
    let mut i = online(InstancePolicy::default());
    step(&mut i, Event::Restart, 5_000);
    assert_eq!(i.state(), State::Stopping);
    assert_eq!(step(&mut i, exit(0), 5_100), vec![Effect::Spawn]);
    assert_eq!(i.state(), State::Starting);
    assert_eq!(i.restarts(), 1);
}

#[test]
fn stop_during_restart_wins() {
    let mut i = online(InstancePolicy::default());
    step(&mut i, Event::Restart, 5_000);
    step(&mut i, Event::Stop, 5_010);
    assert_eq!(step(&mut i, exit(0), 5_100), vec![]);
    assert_eq!(i.state(), State::Stopped);
}

// ------------------------------------------------------------ crash handling

#[test]
fn crash_waits_at_least_min_interval_spawn_to_spawn() {
    let mut i = online(InstancePolicy::default()); // restart_delay 0
    let fx = step(&mut i, exit(1), 300);
    assert_eq!(i.state(), State::Backoff);
    let (_, after, _) = armed(&fx).expect("backoff timer");
    assert_eq!(after, MIN_RESTART_INTERVAL - MS(300));
    assert_eq!(fire(&mut i, 1_000), vec![Effect::Spawn]);
}

#[test]
fn crash_loop_errors_after_max_restarts() {
    let policy = InstancePolicy {
        restart: RestartPolicy {
            max_restarts: 3,
            ..RestartPolicy::default()
        },
        ..InstancePolicy::default()
    };
    let mut i = online(policy);
    let mut now = 0;
    let mut auto_restarts = 0;
    loop {
        now += 10; // dies immediately: unstable
        step(&mut i, exit(1), now);
        if i.state() == State::Errored {
            break;
        }
        now += 1_000;
        assert_eq!(fire(&mut i, now), vec![Effect::Spawn]);
        step(&mut i, Event::Spawned, now);
        auto_restarts += 1;
    }
    assert_eq!(auto_restarts, 3);
    assert_eq!(i.error(), Some(ErrorReason::CrashLoop { unstable_runs: 4 }));
    // An operator start clears the error and the budget.
    assert_eq!(step(&mut i, Event::Start, now + 1), vec![Effect::Spawn]);
    assert_eq!(i.error(), None);
}

#[test]
fn stable_run_resets_crash_budget() {
    let policy = InstancePolicy {
        restart: RestartPolicy {
            max_restarts: 1,
            ..RestartPolicy::default()
        },
        ..InstancePolicy::default()
    };
    let mut i = online(policy);
    step(&mut i, exit(1), 10); // unstable #1
    fire(&mut i, 1_000);
    step(&mut i, Event::Spawned, 1_000);
    step(&mut i, exit(1), 5_000); // ran 4 s: stable, streak reset
    assert_eq!(i.state(), State::Backoff);
    fire(&mut i, 5_000);
    step(&mut i, Event::Spawned, 5_000);
    step(&mut i, exit(1), 5_010); // unstable #1 again, still within budget
    assert_eq!(i.state(), State::Backoff);
}

#[test]
fn exponential_backoff_grows_and_resets_after_long_run() {
    let policy = InstancePolicy {
        restart: RestartPolicy {
            backoff: Backoff::Exponential {
                initial: MS(2_000),
                max: MS(15_000),
            },
            min_uptime: MS(10),
            ..RestartPolicy::default()
        },
        ..InstancePolicy::default()
    };
    let mut i = online(policy);
    let mut now = 0;
    let mut delays = vec![];
    for _ in 0..3 {
        now += 100;
        let fx = step(&mut i, exit(1), now);
        let (_, after, _) = armed(&fx).expect("backoff");
        delays.push(after);
        now += u64::try_from(after.as_millis()).expect("fits");
        fire(&mut i, now);
        step(&mut i, Event::Spawned, now);
    }
    assert_eq!(delays, vec![MS(2_000), MS(3_000), MS(4_500)]);
    now += 31_000; // longer than backoff_reset_after
    let fx = step(&mut i, exit(1), now);
    assert_eq!(armed(&fx).map(|a| a.1), Some(MS(2_000)));
}

#[test]
fn spawn_failure_counts_as_unstable() {
    let policy = InstancePolicy {
        restart: RestartPolicy {
            max_restarts: 0,
            ..RestartPolicy::default()
        },
        ..InstancePolicy::default()
    };
    let mut i = Instance::new(policy);
    step(&mut i, Event::Start, 0);
    step(&mut i, Event::SpawnFailed, 0);
    assert_eq!(i.state(), State::Errored);
    assert_eq!(i.last_exit(), Some(ExitStatus::default()));
}

#[test]
fn autorestart_off_and_stop_exit_codes_stay_stopped() {
    for (autorestart, codes, code) in [(false, vec![], 1), (true, vec![0], 0)] {
        let policy = InstancePolicy {
            restart: RestartPolicy {
                autorestart,
                stop_exit_codes: codes,
                ..RestartPolicy::default()
            },
            ..InstancePolicy::default()
        };
        let mut i = online(policy);
        assert_eq!(step(&mut i, exit(code), 5_000), vec![]);
        assert_eq!(i.state(), State::Stopped);
        assert_eq!(i.desired(), Desired::Stopped);
    }
}

#[test]
fn stop_cancels_pending_restart() {
    let mut i = online(InstancePolicy::default());
    step(&mut i, exit(1), 100);
    let (timer, generation) = i.armed_timer().expect("backoff");
    assert_eq!(step(&mut i, Event::Stop, 200), vec![]);
    assert_eq!(i.state(), State::Stopped);
    assert_eq!(
        step(&mut i, Event::TimerFired { timer, generation }, 1_000),
        vec![]
    );
}

// ----------------------------------------------------------------- readiness

fn with_ready() -> InstancePolicy {
    InstancePolicy {
        ready_timeout: Some(MS(3_000)),
        ..InstancePolicy::default()
    }
}

#[test]
fn readiness_gates_online() {
    let mut i = Instance::new(with_ready());
    let fx = step(&mut i, Event::Start, 0);
    assert_eq!(
        armed(&fx).map(|a| (a.0, a.1)),
        Some((Timer::Ready, MS(3_000)))
    );
    step(&mut i, Event::Spawned, 10);
    assert_eq!(i.state(), State::Starting);
    step(&mut i, Event::Ready, 800);
    assert_eq!(i.state(), State::Online);
}

#[test]
fn readiness_timeout_stops_and_counts_unstable() {
    let mut i = Instance::new(with_ready());
    step(&mut i, Event::Start, 0);
    step(&mut i, Event::Spawned, 10);
    let fx = fire(&mut i, 3_000);
    assert_eq!(fx[0], Effect::SignalStop);
    // Uptime 3.1 s is above min_uptime, but it never became ready.
    step(&mut i, exit(143), 3_100);
    assert_eq!(i.state(), State::Backoff);
    assert_eq!(i.unstable_streak, 1);
}

#[test]
fn exit_before_ready_is_unstable() {
    let mut i = Instance::new(with_ready());
    step(&mut i, Event::Start, 0);
    step(&mut i, Event::Spawned, 10);
    step(&mut i, exit(1), 2_500);
    assert_eq!(i.unstable_streak, 1);
}

// -------------------------------------------------------------------- health

fn with_health() -> InstancePolicy {
    InstancePolicy {
        health: Some(HealthPolicy {
            unhealthy_after: 2,
            restart_after: 4,
        }),
        ..InstancePolicy::default()
    }
}

#[test]
fn health_failures_mark_unhealthy_then_recover() {
    let mut i = online(with_health());
    step(&mut i, Event::Health { ok: false }, 10_000);
    assert_eq!(i.state(), State::Online);
    step(&mut i, Event::Health { ok: false }, 11_000);
    assert_eq!(i.state(), State::Unhealthy);
    step(&mut i, Event::Health { ok: true }, 12_000);
    assert_eq!(i.state(), State::Online);
}

#[test]
fn persistent_health_failure_restarts() {
    let mut i = online(with_health());
    for n in 0..3 {
        step(&mut i, Event::Health { ok: false }, 10_000 + n);
    }
    let fx = step(&mut i, Event::Health { ok: false }, 10_004);
    assert_eq!(fx[0], Effect::SignalStop);
    step(&mut i, exit(143), 10_100);
    assert_eq!(i.state(), State::Backoff);
    assert_eq!(i.unstable_streak, 0, "it ran 10 s: a stable run");
}

// ------------------------------------------------------------------ recovery

#[test]
fn recovery_matrix() {
    let cases = [
        (true, Desired::Running, State::Online, vec![]),
        (
            false,
            Desired::Running,
            State::Starting,
            vec![Effect::Spawn],
        ),
        (false, Desired::Stopped, State::Stopped, vec![]),
    ];
    for (alive, desired, state, fx) in cases {
        let mut i = Instance::recovering(InstancePolicy::default(), desired);
        assert_eq!(step(&mut i, Event::Recovered { alive }, 0), fx);
        assert_eq!(i.state(), state);
    }
    let mut i = Instance::recovering(InstancePolicy::default(), Desired::Stopped);
    let fx = step(&mut i, Event::Recovered { alive: true }, 0);
    assert_eq!(fx[0], Effect::SignalStop);
}

#[test]
fn commands_while_unknown_update_desired_only() {
    let mut i = Instance::recovering(InstancePolicy::default(), Desired::Running);
    assert_eq!(step(&mut i, Event::Stop, 0), vec![]);
    assert_eq!(i.desired(), Desired::Stopped);
    let fx = step(&mut i, Event::Recovered { alive: true }, 1);
    assert_eq!(fx[0], Effect::SignalStop);
}

// -------------------------------------------------------------------- reload

fn with_reload(ready: ReplacementReady) -> InstancePolicy {
    InstancePolicy {
        reload: Some(ready),
        ..InstancePolicy::default()
    }
}

#[test]
fn reload_without_start_first_policy_is_a_restart() {
    let mut i = online(InstancePolicy::default());
    let fx = step(&mut i, Event::Reload, 5_000);
    assert_eq!(fx[0], Effect::SignalStop);
    assert_eq!(i.state(), State::Stopping);
}

#[test]
fn reload_promotes_a_replacement_that_reports_ready() {
    let mut i = online(with_reload(ReplacementReady::Signal(MS(3_000))));
    let fx = step(&mut i, Event::Reload, 5_000);
    assert_eq!(fx[0], Effect::SpawnReplacement);
    assert_eq!(
        armed(&fx).map(|a| (a.0, a.1)),
        Some((Timer::Replacement, MS(3_000)))
    );
    assert_eq!(
        i.state(),
        State::Online,
        "the current process keeps serving"
    );
    assert_eq!(
        step(&mut i, Event::Reload, 5_001),
        vec![],
        "one reload at a time"
    );
    step(&mut i, Event::ReplacementSpawned, 5_010);
    assert_eq!(
        step(&mut i, Event::ReplacementReady, 5_500),
        vec![Effect::PromoteReplacement]
    );
    assert_eq!(i.state(), State::Online);
    assert!(!i.reloading());
    assert_eq!(i.restarts(), 1);
    assert_eq!(
        i.uptime(Millis(6_500)),
        Some(MS(1_000)),
        "uptime counts from promotion"
    );
}

#[test]
fn reload_keeps_the_current_process_when_the_replacement_fails() {
    for failure in [
        Event::ReplacementExited(ExitStatus {
            code: Some(1),
            signal: None,
        }),
        Event::ReplacementSpawnFailed,
    ] {
        let mut i = online(with_reload(ReplacementReady::Signal(MS(3_000))));
        step(&mut i, Event::Reload, 5_000);
        assert_eq!(
            step(&mut i, failure, 5_100),
            vec![Effect::AbandonReplacement]
        );
        assert_eq!(i.state(), State::Online);
        assert!(!i.reloading());
        assert_eq!(i.restarts(), 0);
    }
    // No ready signal in time.
    let mut i = online(with_reload(ReplacementReady::Signal(MS(3_000))));
    step(&mut i, Event::Reload, 5_000);
    assert_eq!(fire(&mut i, 8_000), vec![Effect::AbandonReplacement]);
    assert_eq!(i.state(), State::Online);
}

#[test]
fn stable_replacement_is_promoted_after_its_stable_time() {
    let mut i = online(with_reload(ReplacementReady::Stable(MS(1_000))));
    step(&mut i, Event::Reload, 5_000);
    step(&mut i, Event::ReplacementSpawned, 5_010);
    assert_eq!(
        step(&mut i, Event::ReplacementReady, 5_100),
        vec![],
        "readiness is not used"
    );
    assert_eq!(fire(&mut i, 6_000), vec![Effect::PromoteReplacement]);
}

#[test]
fn interruptions_abandon_the_replacement() {
    let policy = with_reload(ReplacementReady::Signal(MS(3_000)));
    // The current process crashes mid-reload: replacement dropped, crash handled.
    let mut i = online(policy.clone());
    step(&mut i, Event::Reload, 5_000);
    let fx = step(&mut i, exit(1), 5_100);
    assert_eq!(fx[0], Effect::AbandonReplacement);
    assert_eq!(i.state(), State::Backoff);
    // Stop mid-reload.
    let mut i = online(policy.clone());
    step(&mut i, Event::Reload, 5_000);
    let fx = step(&mut i, Event::Stop, 5_100);
    assert_eq!(fx[..2], [Effect::AbandonReplacement, Effect::SignalStop]);
    // Restart mid-reload.
    let mut i = online(policy);
    step(&mut i, Event::Reload, 5_000);
    let fx = step(&mut i, Event::Restart, 5_100);
    assert_eq!(fx[..2], [Effect::AbandonReplacement, Effect::SignalStop]);
}

#[test]
fn reload_of_a_stopped_instance_starts_it() {
    let mut i = Instance::new(with_reload(ReplacementReady::Signal(MS(3_000))));
    assert_eq!(step(&mut i, Event::Reload, 0), vec![Effect::Spawn]);
}

// ---------------------------------------------- exhaustive (state × event)

/// Build an instance in every state, with both policies that change behaviour.
fn every_state() -> Vec<Instance> {
    let mut out = vec![];
    for policy in [
        InstancePolicy::default(),
        with_ready(),
        with_health(),
        with_reload(ReplacementReady::Signal(MS(3_000))),
        with_reload(ReplacementReady::Stable(MS(1_000))),
    ] {
        let fresh = Instance::new(policy.clone());
        out.push(fresh.clone()); // Stopped

        let mut starting = fresh.clone();
        starting.handle(Event::Start, ctx(0));
        out.push(starting.clone());

        let mut on = starting.clone();
        on.handle(Event::Spawned, ctx(0));
        on.handle(Event::Ready, ctx(0));
        out.push(on.clone());

        let mut unhealthy = on.clone();
        for _ in 0..2 {
            unhealthy.handle(Event::Health { ok: false }, ctx(1));
        }
        out.push(unhealthy);

        let mut reloading = on.clone();
        reloading.handle(Event::Reload, ctx(2));
        out.push(reloading);

        let mut stopping = on.clone();
        stopping.handle(Event::Stop, ctx(5_000));
        out.push(stopping.clone());

        let mut killed = stopping.clone();
        fire(&mut killed, 9_000);
        out.push(killed);

        let mut backoff = on.clone();
        backoff.handle(exit(1), ctx(5_000));
        out.push(backoff);

        let mut errored = Instance::new(InstancePolicy {
            restart: RestartPolicy {
                max_restarts: 0,
                ..policy.restart.clone()
            },
            ..policy.clone()
        });
        errored.handle(Event::Start, ctx(0));
        errored.handle(Event::SpawnFailed, ctx(0));
        out.push(errored);

        for d in [Desired::Running, Desired::Stopped] {
            out.push(Instance::recovering(policy.clone(), d));
        }
    }
    out
}

fn every_event(i: &Instance) -> Vec<Event> {
    let mut events = vec![
        Event::Start,
        Event::Stop,
        Event::Restart,
        Event::Spawned,
        Event::SpawnFailed,
        Event::Ready,
        Event::Health { ok: true },
        Event::Health { ok: false },
        exit(0),
        exit(1),
        Event::Exited(ExitStatus {
            code: None,
            signal: Some(9),
        }),
        Event::Recovered { alive: true },
        Event::Recovered { alive: false },
        Event::Reload,
        Event::ReplacementSpawned,
        Event::ReplacementSpawnFailed,
        Event::ReplacementReady,
        Event::ReplacementExited(ExitStatus::default()),
    ];
    for timer in [
        Timer::Ready,
        Timer::Backoff,
        Timer::Stop,
        Timer::Replacement,
    ] {
        events.push(Event::TimerFired {
            timer,
            generation: 0,
        });
        events.push(Event::TimerFired {
            timer,
            generation: u64::MAX,
        });
    }
    if let Some((timer, generation)) = i.armed_timer() {
        events.push(Event::TimerFired { timer, generation });
    }
    events
}

#[test]
fn every_event_in_every_state_keeps_invariants() {
    let states = every_state();
    let covered: std::collections::HashSet<_> = states.iter().map(Instance::state).collect();
    assert_eq!(covered.len(), 8, "all states reached: {covered:?}");
    for base in states {
        check_invariants(&base);
        for e in every_event(&base) {
            let mut i = base.clone();
            let fx = i.handle(e, ctx(20_000));
            check_invariants(&i);
            check_effects(&i, &fx);
        }
    }
}

#[test]
fn stale_timers_never_have_effects() {
    for base in every_state() {
        for timer in [
            Timer::Ready,
            Timer::Backoff,
            Timer::Stop,
            Timer::Replacement,
        ] {
            for generation in [0, base.generation + 1, u64::MAX] {
                let mut i = base.clone();
                let fx = i.handle(Event::TimerFired { timer, generation }, ctx(20_000));
                assert_eq!(fx, vec![]);
                assert_eq!(i, base);
            }
        }
    }
}

// ------------------------------------------------------------ property tests

fn arb_policy() -> impl Strategy<Value = InstancePolicy> {
    (
        prop_oneof![
            (0u64..5_000).prop_map(|d| Backoff::Fixed(MS(d))),
            (0u64..5_000, 0u64..20_000).prop_map(|(a, b)| Backoff::Exponential {
                initial: MS(a),
                max: MS(b),
            }),
        ],
        any::<bool>(),
        0u64..3_000,
        0u32..20,
        proptest::option::of(1u64..5_000),
        proptest::option::of((any::<bool>(), 1u64..5_000)),
    )
        .prop_map(
            |(backoff, jitter, min_uptime, max_restarts, ready, reload)| InstancePolicy {
                restart: RestartPolicy {
                    backoff,
                    jitter: if jitter { Jitter::Equal } else { Jitter::None },
                    min_uptime: MS(min_uptime),
                    max_restarts,
                    ..RestartPolicy::default()
                },
                ready_timeout: ready.map(MS),
                health: Some(HealthPolicy {
                    unhealthy_after: 1,
                    restart_after: 3,
                }),
                reload: reload.map(|(signal, ms)| {
                    if signal {
                        ReplacementReady::Signal(MS(ms))
                    } else {
                        ReplacementReady::Stable(MS(ms))
                    }
                }),
                ..InstancePolicy::default()
            },
        )
}

fn arb_event() -> impl Strategy<Value = Event> {
    prop_oneof![
        Just(Event::Start),
        Just(Event::Stop),
        Just(Event::Restart),
        Just(Event::Spawned),
        Just(Event::SpawnFailed),
        Just(Event::Ready),
        any::<bool>().prop_map(|ok| Event::Health { ok }),
        (proptest::option::of(0i32..3))
            .prop_map(|code| Event::Exited(ExitStatus { code, signal: None })),
        (0u8..4, 0u64..40).prop_map(|(t, generation)| Event::TimerFired {
            timer: [
                Timer::Ready,
                Timer::Backoff,
                Timer::Stop,
                Timer::Replacement
            ][usize::from(t)],
            generation,
        }),
        any::<bool>().prop_map(|alive| Event::Recovered { alive }),
        Just(Event::Reload),
        Just(Event::ReplacementSpawned),
        Just(Event::ReplacementSpawnFailed),
        Just(Event::ReplacementReady),
        Just(Event::ReplacementExited(ExitStatus::default())),
    ]
}

use crate::restart::Jitter;

proptest! {
    /// Any sequence of events, in any order, at any times: no panic, and the
    /// invariants hold after every step.
    #[test]
    fn arbitrary_events_keep_invariants(
        policy in arb_policy(),
        events in proptest::collection::vec((arb_event(), 0u64..10_000, any::<u64>()), 0..200),
    ) {
        let mut i = Instance::new(policy);
        let mut now = 0u64;
        for (e, dt, rand) in events {
            now += dt;
            let fx = i.handle(e, Ctx { now: Millis(now), rand });
            check_invariants(&i);
            check_effects(&i, &fx);
        }
    }

    /// Without operator commands, automatic spawns of one instance are at
    /// least MIN_RESTART_INTERVAL apart, and a run of unstable exits ends in
    /// `Errored` after exactly `max_restarts` automatic restarts.
    #[test]
    fn automatic_restarts_are_rate_limited_and_bounded(
        policy in arb_policy(),
        runs in proptest::collection::vec((0u64..5_000, any::<bool>(), any::<u64>()), 1..60),
    ) {
        let mut i = Instance::new(policy.clone());
        let mut now = 0u64;
        let mut spawn_times = vec![now];
        let mut streak = 0u32;
        step(&mut i, Event::Start, now);
        for (uptime, ready, rand) in runs {
            prop_assert_eq!(i.state(), State::Starting);
            step(&mut i, Event::Spawned, now);
            if ready {
                step(&mut i, Event::Ready, now);
            }
            // The readiness timer, if still armed, may fire before the exit.
            let ready_deadline = match i.armed_timer() {
                Some((Timer::Ready, _)) => policy.ready_timeout.map(|t| t.as_millis()),
                _ => None,
            };
            let never_ready = ready_deadline.is_some();
            if let Some(deadline) = ready_deadline
                && u128::from(uptime) >= deadline
            {
                now += u64::try_from(deadline).expect("fits");
                fire(&mut i, now);
                prop_assert_eq!(i.state(), State::Stopping);
            } else {
                now = spawn_times.last().copied().unwrap_or(0) + uptime;
            }
            let ran = Millis(now).since(Millis(*spawn_times.last().unwrap_or(&0)));
            let fx = i.handle(exit(1), Ctx { now: Millis(now), rand });
            check_invariants(&i);
            check_effects(&i, &fx);

            if never_ready || ran < policy.restart.min_uptime {
                streak += 1;
            } else {
                streak = 0;
            }
            if streak > policy.restart.max_restarts {
                prop_assert_eq!(i.state(), State::Errored);
                break;
            }
            prop_assert_eq!(i.state(), State::Backoff);
            let (_, after, _) = armed(&fx).expect("backoff timer");
            now += u64::try_from(after.as_millis()).expect("fits");
            let fx = fire(&mut i, now);
            prop_assert_eq!(fx.first(), Some(&Effect::Spawn));
            let prev = *spawn_times.last().unwrap_or(&0);
            prop_assert!(
                Millis(now).since(Millis(prev)) >= MIN_RESTART_INTERVAL,
                "spawns at {} and {} are closer than the floor", prev, now
            );
            spawn_times.push(now);
        }
    }
}

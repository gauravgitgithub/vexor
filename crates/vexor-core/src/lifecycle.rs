//! Per-instance lifecycle state machine.
//!
//! [`Instance::handle`] is the only place lifecycle decisions are made. It
//! takes an [`Event`] and the current time, updates the state, and returns the
//! [`Effect`]s the caller must carry out (spawn, signal, arm a timer). Every
//! `(state, event)` pair is handled; pairs that make no sense in a state are
//! ignored and return no effects, so stale or duplicate events are harmless.

use std::time::Duration;

use serde::Serialize;

use crate::restart::RestartPolicy;
use crate::time::Millis;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Stopped,
    /// Spawn requested; waiting for the process to start and, if readiness is
    /// configured, to report ready.
    Starting,
    Online,
    /// Running, but health checks are failing.
    Unhealthy,
    /// Graceful stop sent; waiting for exit (force-killed after `kill_timeout`).
    Stopping,
    /// Exited; waiting out the restart delay.
    Backoff,
    /// Crash loop: too many consecutive unstable runs. Needs an operator
    /// `start`/`restart`.
    Errored,
    /// After a daemon restart, before the recorded process is checked.
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Desired {
    Running,
    Stopped,
}

/// How a process ended. Both fields are `None` when it never started.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
pub struct ExitStatus {
    pub code: Option<i32>,
    pub signal: Option<i32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Timer {
    /// Readiness deadline (PM2 `listen_timeout`).
    Ready,
    /// Restart delay.
    Backoff,
    /// Graceful-stop deadline (PM2 `kill_timeout`).
    Stop,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    /// Operator `start`.
    Start,
    /// Operator `stop`.
    Stop,
    /// Operator `restart`.
    Restart,
    /// The process was created.
    Spawned,
    /// The process could not be created (missing executable, permissions, …).
    SpawnFailed,
    /// The app reported ready (`process.send('ready')`, or a passing health
    /// check while starting).
    Ready,
    /// Result of a periodic health check.
    Health {
        ok: bool,
    },
    Exited(ExitStatus),
    /// A timer armed by [`Effect::ArmTimer`] fired. Ignored unless it matches
    /// the currently armed timer and generation.
    TimerFired {
        timer: Timer,
        generation: u64,
    },
    /// Outcome of checking the recorded process after a daemon restart.
    Recovered {
        alive: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effect {
    Spawn,
    /// Ask the process to stop (shutdown message or SIGTERM to its group).
    SignalStop,
    /// SIGKILL the process group.
    ForceKill,
    /// Arm `timer`, replacing any armed timer. Deliver
    /// [`Event::TimerFired`] with the same `generation` when it expires.
    ArmTimer {
        timer: Timer,
        after: Duration,
        generation: u64,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum ErrorReason {
    /// `unstable_runs` consecutive runs were shorter than `min_uptime` (or
    /// never became ready).
    CrashLoop { unstable_runs: u32 },
}

/// What to do once a stopping process exits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AfterStop {
    /// Stay stopped.
    Stay,
    /// Operator restart: spawn again immediately.
    Respawn,
    /// Stopped by Vexor because it failed readiness or health; treat the exit
    /// as a crash. `unstable` forces it to count against the crash budget.
    Recover { unstable: bool },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HealthPolicy {
    /// Consecutive failures before `Online` → `Unhealthy`.
    pub unhealthy_after: u32,
    /// Consecutive failures before the instance is restarted.
    pub restart_after: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstancePolicy {
    pub restart: RestartPolicy,
    /// When set, an instance stays `Starting` until [`Event::Ready`]; if that
    /// doesn't arrive within the timeout it is stopped and counted as an
    /// unstable run. When unset, it is `Online` as soon as it spawns.
    pub ready_timeout: Option<Duration>,
    /// PM2 `kill_timeout`.
    pub kill_timeout: Duration,
    pub health: Option<HealthPolicy>,
}

impl Default for InstancePolicy {
    fn default() -> Self {
        InstancePolicy {
            restart: RestartPolicy::default(),
            ready_timeout: None,
            kill_timeout: Duration::from_millis(1600), // PM2 default
            health: None,
        }
    }
}

/// Inputs from the outside world that accompany every event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ctx {
    pub now: Millis,
    /// Uniformly random value, used only for restart jitter.
    pub rand: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Instance {
    policy: InstancePolicy,
    state: State,
    desired: Desired,
    after_stop: AfterStop,
    /// Currently armed timer and its generation.
    timer: Option<(Timer, u64)>,
    generation: u64,
    started_at: Option<Millis>,
    health_failures: u32,
    /// Consecutive unstable runs (crash budget).
    unstable_streak: u32,
    /// Automatic restarts since the exponential backoff last reset.
    backoff_attempt: u32,
    spawns: u64,
    last_exit: Option<ExitStatus>,
    error: Option<ErrorReason>,
}

impl Instance {
    pub fn new(policy: InstancePolicy) -> Self {
        Instance {
            policy,
            state: State::Stopped,
            desired: Desired::Stopped,
            after_stop: AfterStop::Stay,
            timer: None,
            generation: 0,
            started_at: None,
            health_failures: 0,
            unstable_streak: 0,
            backoff_attempt: 0,
            spawns: 0,
            last_exit: None,
            error: None,
        }
    }

    /// An instance recorded before a daemon restart, whose process has not
    /// been checked yet. Send [`Event::Recovered`] once it has.
    pub fn recovering(policy: InstancePolicy, desired: Desired) -> Self {
        Instance {
            state: State::Unknown,
            desired,
            ..Instance::new(policy)
        }
    }

    pub fn state(&self) -> State {
        self.state
    }

    pub fn desired(&self) -> Desired {
        self.desired
    }

    pub fn policy(&self) -> &InstancePolicy {
        &self.policy
    }

    /// Spawns after the first (automatic and operator-initiated).
    pub fn restarts(&self) -> u64 {
        self.spawns.saturating_sub(1)
    }

    pub fn last_exit(&self) -> Option<ExitStatus> {
        self.last_exit
    }

    pub fn error(&self) -> Option<ErrorReason> {
        self.error
    }

    pub fn armed_timer(&self) -> Option<(Timer, u64)> {
        self.timer
    }

    pub fn uptime(&self, now: Millis) -> Option<Duration> {
        match self.state {
            State::Starting | State::Online | State::Unhealthy | State::Stopping => {
                self.started_at.map(|t| now.since(t))
            }
            _ => None,
        }
    }

    pub fn handle(&mut self, event: Event, ctx: Ctx) -> Vec<Effect> {
        let mut fx = Vec::new();
        use State as S;
        match event {
            Event::Start => {
                self.desired = Desired::Running;
                match self.state {
                    S::Stopped | S::Errored | S::Backoff => {
                        self.reset_budget();
                        self.spawn(ctx.now, &mut fx);
                    }
                    S::Stopping => self.after_stop = AfterStop::Respawn,
                    S::Starting | S::Online | S::Unhealthy | S::Unknown => {}
                }
            }
            Event::Stop => {
                self.desired = Desired::Stopped;
                match self.state {
                    S::Starting | S::Online | S::Unhealthy => {
                        self.begin_stop(AfterStop::Stay, &mut fx);
                    }
                    S::Stopping => self.after_stop = AfterStop::Stay,
                    S::Backoff | S::Errored => {
                        self.timer = None;
                        self.error = None;
                        self.state = S::Stopped;
                    }
                    S::Stopped | S::Unknown => {}
                }
            }
            Event::Restart => {
                self.desired = Desired::Running;
                match self.state {
                    S::Starting | S::Online | S::Unhealthy => {
                        self.begin_stop(AfterStop::Respawn, &mut fx);
                    }
                    S::Stopping => self.after_stop = AfterStop::Respawn,
                    S::Stopped | S::Errored | S::Backoff => {
                        self.reset_budget();
                        self.spawn(ctx.now, &mut fx);
                    }
                    S::Unknown => {}
                }
            }
            Event::Spawned => {
                if self.state == S::Starting && self.policy.ready_timeout.is_none() {
                    self.state = S::Online;
                }
            }
            Event::SpawnFailed => {
                if self.state == S::Starting {
                    self.on_exit(ExitStatus::default(), true, ctx, &mut fx);
                }
            }
            Event::Ready => {
                if self.state == S::Starting {
                    self.timer = None;
                    self.state = S::Online;
                }
            }
            Event::Health { ok } => self.on_health(ok, &mut fx),
            Event::Exited(status) => match self.state {
                S::Starting | S::Online | S::Unhealthy => {
                    // Exiting before reporting ready is never a stable run.
                    let unstable = self.state == S::Starting && self.policy.ready_timeout.is_some();
                    self.on_exit(status, unstable, ctx, &mut fx);
                }
                S::Stopping => {
                    let after = std::mem::replace(&mut self.after_stop, AfterStop::Stay);
                    match after {
                        AfterStop::Stay => {
                            self.timer = None;
                            self.last_exit = Some(status);
                            self.state = S::Stopped;
                        }
                        AfterStop::Respawn => {
                            self.last_exit = Some(status);
                            self.reset_budget();
                            self.spawn(ctx.now, &mut fx);
                        }
                        AfterStop::Recover { unstable } => {
                            self.on_exit(status, unstable, ctx, &mut fx);
                        }
                    }
                }
                S::Stopped | S::Backoff | S::Errored | S::Unknown => {}
            },
            Event::TimerFired { timer, generation } => {
                if self.timer != Some((timer, generation)) {
                    return fx;
                }
                self.timer = None;
                match (timer, self.state) {
                    (Timer::Ready, S::Starting) => {
                        self.begin_stop(AfterStop::Recover { unstable: true }, &mut fx);
                    }
                    (Timer::Backoff, S::Backoff) => self.spawn(ctx.now, &mut fx),
                    (Timer::Stop, S::Stopping) => fx.push(Effect::ForceKill),
                    _ => {}
                }
            }
            Event::Recovered { alive } => {
                if self.state != S::Unknown {
                    return fx;
                }
                match (alive, self.desired) {
                    (true, Desired::Running) => {
                        // True start time is unknown; uptime counts from adoption.
                        self.started_at = Some(ctx.now);
                        self.spawns = 1;
                        self.state = S::Online;
                    }
                    (true, Desired::Stopped) => {
                        self.started_at = Some(ctx.now);
                        self.begin_stop(AfterStop::Stay, &mut fx);
                    }
                    (false, Desired::Running) => self.spawn(ctx.now, &mut fx),
                    (false, Desired::Stopped) => self.state = S::Stopped,
                }
            }
        }
        fx
    }

    fn reset_budget(&mut self) {
        self.unstable_streak = 0;
        self.backoff_attempt = 0;
        self.error = None;
    }

    fn arm(&mut self, timer: Timer, after: Duration, fx: &mut Vec<Effect>) {
        self.generation += 1;
        self.timer = Some((timer, self.generation));
        fx.push(Effect::ArmTimer {
            timer,
            after,
            generation: self.generation,
        });
    }

    fn spawn(&mut self, now: Millis, fx: &mut Vec<Effect>) {
        self.timer = None;
        self.state = State::Starting;
        self.after_stop = AfterStop::Stay;
        self.started_at = Some(now);
        self.health_failures = 0;
        self.spawns += 1;
        fx.push(Effect::Spawn);
        if let Some(t) = self.policy.ready_timeout {
            self.arm(Timer::Ready, t, fx);
        }
    }

    fn begin_stop(&mut self, after: AfterStop, fx: &mut Vec<Effect>) {
        self.state = State::Stopping;
        self.after_stop = after;
        fx.push(Effect::SignalStop);
        self.arm(Timer::Stop, self.policy.kill_timeout, fx);
    }

    fn on_health(&mut self, ok: bool, fx: &mut Vec<Effect>) {
        let Some(hp) = self.policy.health else {
            return;
        };
        if !matches!(self.state, State::Online | State::Unhealthy) {
            return;
        }
        if ok {
            self.health_failures = 0;
            self.state = State::Online;
            return;
        }
        self.health_failures = self.health_failures.saturating_add(1);
        if self.health_failures >= hp.restart_after {
            self.begin_stop(AfterStop::Recover { unstable: false }, fx);
        } else if self.health_failures >= hp.unhealthy_after {
            self.state = State::Unhealthy;
        }
    }

    /// The process ended on its own, or after Vexor stopped it for failing
    /// readiness/health. Decide between stopped, errored, and backoff.
    fn on_exit(
        &mut self,
        status: ExitStatus,
        force_unstable: bool,
        ctx: Ctx,
        fx: &mut Vec<Effect>,
    ) {
        self.timer = None;
        self.last_exit = Some(status);
        let uptime = self.started_at.map_or(Duration::ZERO, |t| ctx.now.since(t));
        let rp = &self.policy.restart;

        let wants_stop = status.code.is_some_and(|c| rp.is_stop_exit_code(c));
        if !rp.autorestart || wants_stop {
            self.desired = Desired::Stopped;
            self.state = State::Stopped;
            return;
        }

        if force_unstable || uptime < rp.min_uptime {
            self.unstable_streak = self.unstable_streak.saturating_add(1);
        } else {
            self.unstable_streak = 0;
        }
        if uptime >= rp.backoff_reset_after {
            self.backoff_attempt = 0;
        }
        if self.unstable_streak > rp.max_restarts {
            self.error = Some(ErrorReason::CrashLoop {
                unstable_runs: self.unstable_streak,
            });
            self.state = State::Errored;
            return;
        }

        let delay = rp.delay(self.backoff_attempt, uptime, ctx.rand);
        self.backoff_attempt = self.backoff_attempt.saturating_add(1);
        self.state = State::Backoff;
        self.arm(Timer::Backoff, delay, fx);
    }
}

#[cfg(test)]
mod tests;

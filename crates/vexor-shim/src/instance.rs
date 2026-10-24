//! Runs one app instance: feeds real-world events into the core state machine
//! and carries out the effects it returns.
//!
//! An instance has a *current* process and, during a start-first reload, a
//! *replacement* (docs/architecture.md §6a). In fork mode each is a child of
//! the shim; in cluster mode each is a worker of the shared cluster primary.
//! A process that is replaced, or a replacement that is abandoned, is
//! *retired*: stopped gracefully by a separate task, without its exit being
//! reported to the state machine.

use std::collections::{BTreeSet, VecDeque};
use std::hash::BuildHasher;
use std::sync::Arc;
use std::time::Duration;

use rustix::process::Signal;
use tokio::sync::{broadcast, mpsc, watch};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use vexor_core::{AppSpec, Ctx, Effect, Event, ExitStatus, Instance, Millis, State, Timer};

use crate::cluster::{Cluster, WorkerEvent, WorkerMsg};
use crate::identity::Identity;
use crate::logs::Sink;
use crate::memory;
use crate::orphan::{self, Managed};
use crate::output::{Output, ShimEvent};
use crate::process::{self, IpcChannel, IpcMessage};

/// How long to wait for log pipes to drain after the instance has finished.
/// A grandchild that inherited stdout can hold a pipe open indefinitely.
const LOG_DRAIN_TIMEOUT: Duration = Duration::from_secs(2);
/// After SIGKILL, how long to wait for the group to disappear.
const KILL_CONFIRM_TIMEOUT: Duration = Duration::from_secs(2);
const POLL: Duration = Duration::from_millis(25);

/// Operator commands for every instance of the app.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Control {
    Restart,
    Reload,
}

/// A cluster worker as this instance knows it.
#[derive(Debug, Clone, Copy)]
struct Worker {
    wid: u32,
    pid: Option<u32>,
    identity: Option<Identity>,
}

/// A forked child as this instance knows it (the `Child` and its IPC
/// channel are separate fields of the runner so `select!` can borrow them
/// independently).
#[derive(Debug, Clone, Copy)]
struct Forked {
    pid: u32,
    identity: Option<Identity>,
}

pub struct Runner {
    spec: Arc<AppSpec>,
    index: u32,
    inst: Instance,
    out: Output,
    epoch: Instant,
    managed: Managed,
    out_log: Sink,
    err_log: Sink,
    cluster: Option<(Cluster, mpsc::UnboundedReceiver<WorkerMsg>)>,

    cur_child: Option<tokio::process::Child>,
    cur_ipc: Option<IpcChannel>,
    cur_fork: Option<Forked>,
    cur_worker: Option<Worker>,

    rep_child: Option<tokio::process::Child>,
    rep_ipc: Option<IpcChannel>,
    rep_fork: Option<Forked>,
    rep_worker: Option<Worker>,
    /// Workers abandoned before they reported `spawned`; stopped on arrival.
    abandoned: BTreeSet<u32>,

    timer: Option<(Timer, u64, Instant)>,
    log_tasks: Vec<JoinHandle<()>>,
    retiring: Vec<JoinHandle<()>>,
    queue: VecDeque<Event>,
    rand: std::collections::hash_map::RandomState,
    rand_counter: u64,
    /// Periodic memory check, while `max_memory_bytes` is set.
    memory_check: Option<tokio::time::Interval>,
    /// HTTP/TCP health check (fork mode; cluster mode checks the app as a
    /// whole, in `lib.rs`).
    health: Option<HealthRun>,
}

type ProbeResult = (u32, Result<(), String>);

struct HealthRun {
    target: vexor_core::HealthTarget,
    interval: Duration,
    timeout: Duration,
    next: Instant,
    busy: bool,
    tx: mpsc::UnboundedSender<ProbeResult>,
    rx: mpsc::UnboundedReceiver<ProbeResult>,
    /// Last result reported for `pid`.
    last: Option<(u32, bool)>,
}

impl Runner {
    pub fn new(
        spec: Arc<AppSpec>,
        index: u32,
        out: Output,
        epoch: Instant,
        managed: Managed,
        (out_log, err_log): (Sink, Sink),
        cluster: Option<Cluster>,
    ) -> Self {
        let inst = Instance::new(spec.instance_policy());
        let cluster = cluster.map(|c| {
            let rx = c.subscribe(index);
            (c, rx)
        });
        Runner {
            spec,
            index,
            inst,
            out,
            epoch,
            managed,
            out_log,
            err_log,
            cluster,
            cur_child: None,
            cur_ipc: None,
            cur_fork: None,
            cur_worker: None,
            rep_child: None,
            rep_ipc: None,
            rep_fork: None,
            rep_worker: None,
            abandoned: BTreeSet::new(),
            timer: None,
            log_tasks: Vec::new(),
            retiring: Vec::new(),
            queue: VecDeque::new(),
            rand: std::collections::hash_map::RandomState::new(),
            rand_counter: 0,
            memory_check: None,
            health: None,
        }
    }

    /// Start the instance and supervise it. Ends once `shutdown` turns true
    /// and the instance has stopped, or, when `exit_when_idle`, as soon as it
    /// is stopped or errored. `control` delivers operator restarts/reloads.
    pub async fn run(
        mut self,
        mut shutdown: watch::Receiver<bool>,
        mut control: broadcast::Receiver<Control>,
        exit_when_idle: bool,
    ) -> State {
        self.queue.push_back(Event::Start);
        if self.spec.policy.max_memory_bytes.is_some() {
            let period = Duration::from_millis(self.spec.policy.memory_check_interval_ms.max(100));
            let mut tick = tokio::time::interval_at(Instant::now() + period, period);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            self.memory_check = Some(tick);
        }
        if let (Some(hc), None) = (&self.spec.health_check, &self.cluster) {
            match hc.target(Some(self.index)) {
                Ok(target) => {
                    let (tx, rx) = mpsc::unbounded_channel();
                    self.health = Some(HealthRun {
                        target,
                        interval: Duration::from_millis(hc.interval_ms.max(100)),
                        timeout: Duration::from_millis(hc.timeout_ms.max(10)),
                        next: Instant::now(),
                        busy: false,
                        tx,
                        rx,
                        last: None,
                    });
                }
                Err(e) => eprintln!("vexor shim: health check disabled: {e}"),
            }
        }
        let mut shutting_down = false;
        let mut control_open = true;
        loop {
            while let Some(event) = self.queue.pop_front() {
                self.dispatch(event);
            }
            let idle =
                !self.alive() && matches!(self.inst.state(), State::Stopped | State::Errored);
            if idle && (exit_when_idle || shutting_down) {
                break;
            }
            let deadline = self.timer.map(|(_, _, at)| at);
            let health_due = self.health_due();
            tokio::select! {
                status = wait_child(&mut self.cur_child), if self.cur_child.is_some() => {
                    self.cur_child = None;
                    self.cur_ipc = None;
                    if let Some(f) = self.cur_fork.take() {
                        self.managed.remove(f.pid);
                        if self.spec.treekill {
                            self.clean_group(f.pid).await;
                        }
                    }
                    self.queue.push_back(Event::Exited(status));
                }
                status = wait_child(&mut self.rep_child), if self.rep_child.is_some() => {
                    self.rep_child = None;
                    self.rep_ipc = None;
                    if let Some(f) = self.rep_fork.take() {
                        self.managed.remove(f.pid);
                        // A failed replacement never served: don't wait.
                        let _ = process::signal_group(f.pid, Signal::KILL);
                    }
                    self.queue.push_back(Event::ReplacementExited(status));
                }
                () = sleep_until(deadline), if deadline.is_some() => {
                    if let Some((timer, generation, _)) = self.timer.take() {
                        self.queue.push_back(Event::TimerFired { timer, generation });
                    }
                }
                msg = recv_ipc(&mut self.cur_ipc), if self.cur_ipc.is_some() => match msg {
                    Some(IpcMessage::Ready) => self.queue.push_back(Event::Ready),
                    Some(IpcMessage::Other) => {}
                    None => self.cur_ipc = None,
                },
                msg = recv_ipc(&mut self.rep_ipc), if self.rep_ipc.is_some() => match msg {
                    Some(IpcMessage::Ready) => self.queue.push_back(Event::ReplacementReady),
                    Some(IpcMessage::Other) => {}
                    None => self.rep_ipc = None,
                },
                msg = recv_worker(&mut self.cluster), if self.cluster.is_some() => match msg {
                    Some(m) => self.on_worker(m),
                    None => self.cluster = None,
                },
                changed = shutdown.changed(), if !shutting_down => {
                    if changed.is_err() || *shutdown.borrow() {
                        shutting_down = true;
                        self.queue.push_back(Event::Stop);
                    }
                }
                () = sleep_until(health_due), if !shutting_down && health_due.is_some() => {
                    self.start_probe();
                }
                r = recv_probe(&mut self.health), if self.health.is_some() => {
                    if let Some(r) = r {
                        self.on_probe(r);
                    }
                }
                () = memory_tick(&mut self.memory_check), if self.memory_check.is_some() && !shutting_down => {
                    self.check_memory();
                }
                c = control.recv(), if control_open && !shutting_down => match c {
                    Ok(Control::Restart) => self.queue.push_back(Event::Restart),
                    Ok(Control::Reload) => self.queue.push_back(Event::Reload),
                    Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(broadcast::error::RecvError::Closed) => control_open = false,
                },
            }
        }
        self.finish_retiring().await;
        self.drain_logs().await;
        self.inst.state()
    }

    /// The current process is running.
    fn alive(&self) -> bool {
        self.cur_child.is_some() || self.cur_worker.is_some_and(|w| w.pid.is_some())
    }

    fn current_pid(&self) -> Option<u32> {
        self.cur_fork
            .map(|f| f.pid)
            .or_else(|| self.cur_worker.and_then(|w| w.pid))
    }

    /// When the next health check is due: only while a process is starting
    /// or running, one check at a time.
    fn health_due(&self) -> Option<Instant> {
        let h = self.health.as_ref()?;
        let running = matches!(
            self.inst.state(),
            State::Starting | State::Online | State::Unhealthy
        );
        (running && !h.busy && self.cur_fork.is_some()).then_some(h.next)
    }

    fn start_probe(&mut self) {
        let (Some(h), Some(f)) = (self.health.as_mut(), self.cur_fork) else {
            return;
        };
        h.busy = true;
        let (target, timeout, tx, pid) = (h.target.clone(), h.timeout, h.tx.clone(), f.pid);
        tokio::spawn(async move {
            let r = crate::health::probe(&target, timeout).await;
            let _ = tx.send((pid, r));
        });
    }

    fn on_probe(&mut self, (pid, result): ProbeResult) {
        let state = self.inst.state();
        let current = self.cur_fork.map(|f| f.pid);
        let Some(h) = self.health.as_mut() else {
            return;
        };
        h.busy = false;
        let ok = result.is_ok();
        // While starting, check often so the app is online soon after it
        // first passes.
        let wait = if state == State::Starting {
            h.interval.min(Duration::from_millis(500))
        } else {
            h.interval
        };
        h.next = Instant::now() + wait;
        if current != Some(pid) {
            // A result for a process that's gone.
            return;
        }
        if h.last != Some((pid, ok)) && !(state == State::Starting && !ok) {
            h.last = Some((pid, ok));
            self.out.emit(&ShimEvent::Health {
                app: &self.spec.name,
                instance: Some(self.index),
                ok,
                detail: result.err(),
                action: None,
            });
        }
        match state {
            State::Starting if ok => self.queue.push_back(Event::Ready),
            State::Online | State::Unhealthy => self.queue.push_back(Event::Health { ok }),
            _ => {}
        }
    }

    /// Restart the instance if its process uses more memory than allowed
    /// (PM2 `max_memory_restart`). Only an online process that isn't being
    /// reloaded is checked.
    fn check_memory(&mut self) {
        let Some(limit) = self.spec.policy.max_memory_bytes else {
            return;
        };
        if self.inst.state() != State::Online || self.inst.reloading() {
            return;
        }
        let (Some(pid), Some(id)) = (self.current_pid(), self.current_identity()) else {
            return;
        };
        // The PID must still be the process we started.
        if !id.is_alive() {
            return;
        }
        let Ok(rss) = memory::rss_bytes(pid) else {
            return;
        };
        if rss > limit {
            self.out.emit(&ShimEvent::MemoryRestart {
                app: &self.spec.name,
                instance: self.index,
                pid,
                rss_bytes: rss,
                limit_bytes: limit,
            });
            self.queue.push_back(Event::Restart);
        }
    }

    fn current_identity(&self) -> Option<Identity> {
        self.cur_fork
            .and_then(|f| f.identity)
            .or_else(|| self.cur_worker.and_then(|w| w.identity))
    }

    fn on_worker(&mut self, msg: WorkerMsg) {
        let WorkerMsg { wid, ev } = msg;
        let is_current = self.cur_worker.is_some_and(|w| w.wid == wid);
        let is_replacement = self.rep_worker.is_some_and(|w| w.wid == wid);
        // A worker given up on before it started: stop it once it exists.
        if self.abandoned.contains(&wid) {
            match ev {
                WorkerEvent::Spawned { .. } => self.retire_worker(wid),
                WorkerEvent::Exited(_) | WorkerEvent::SpawnFailed(_) => {
                    self.abandoned.remove(&wid);
                }
                _ => {}
            }
            return;
        }
        if is_current {
            match ev {
                WorkerEvent::Spawned { pid } => {
                    self.cur_worker = Some(Worker {
                        wid,
                        pid: Some(pid),
                        identity: Identity::of(pid).ok(),
                    });
                    self.queue.push_back(Event::Spawned);
                }
                WorkerEvent::SpawnFailed(error) => {
                    self.cur_worker = None;
                    self.spawn_error(error);
                    self.queue.push_back(Event::SpawnFailed);
                }
                WorkerEvent::Ready => self.queue.push_back(Event::Ready),
                WorkerEvent::Listening => {}
                WorkerEvent::Exited(status) => {
                    self.cur_worker = None;
                    self.queue.push_back(Event::Exited(status));
                }
            }
        } else if is_replacement {
            // Without `wait_ready`, a cluster worker is ready once listening.
            let wait_ready = self.inst.policy().ready_timeout.is_some();
            match ev {
                WorkerEvent::Spawned { pid } => {
                    self.rep_worker = Some(Worker {
                        wid,
                        pid: Some(pid),
                        identity: Identity::of(pid).ok(),
                    });
                    self.queue.push_back(Event::ReplacementSpawned);
                }
                WorkerEvent::SpawnFailed(error) => {
                    self.rep_worker = None;
                    self.spawn_error(error);
                    self.queue.push_back(Event::ReplacementSpawnFailed);
                }
                WorkerEvent::Ready if wait_ready => {
                    self.queue.push_back(Event::ReplacementReady);
                }
                WorkerEvent::Listening if !wait_ready => {
                    self.queue.push_back(Event::ReplacementReady);
                }
                WorkerEvent::Ready | WorkerEvent::Listening => {}
                WorkerEvent::Exited(status) => {
                    self.rep_worker = None;
                    self.queue.push_back(Event::ReplacementExited(status));
                }
            }
        }
        // Anything else is about a retiring worker: nothing to do.
    }

    fn now(&self) -> Millis {
        Millis(u64::try_from(self.epoch.elapsed().as_millis()).unwrap_or(u64::MAX))
    }

    fn ctx(&mut self) -> Ctx {
        self.rand_counter += 1;
        Ctx {
            now: self.now(),
            rand: self.rand.hash_one(self.rand_counter),
        }
    }

    fn dispatch(&mut self, event: Event) {
        let before = self.inst.state();
        let ctx = self.ctx();
        let effects = self.inst.handle(event, ctx);
        let mut promoted = false;
        for effect in effects {
            promoted |= effect == Effect::PromoteReplacement;
            self.apply(effect);
        }
        if self.inst.armed_timer().is_none() {
            self.timer = None;
        }
        // A promotion keeps the state `online` but changes the process.
        if self.inst.state() != before || promoted {
            self.emit_state();
        }
    }

    fn apply(&mut self, effect: Effect) {
        match effect {
            Effect::Spawn => {
                if let Some((c, _)) = &self.cluster {
                    // `Spawned`/`SpawnFailed` arrives as a worker event.
                    let wid = c.spawn(self.index);
                    self.cur_worker = Some(Worker {
                        wid,
                        pid: None,
                        identity: None,
                    });
                    return;
                }
                match process::spawn(&self.spec, self.index, &self.out_log, &self.err_log) {
                    Ok(s) => {
                        let identity = Identity::of(s.pid).ok();
                        self.managed.insert(s.pid, identity);
                        self.cur_fork = Some(Forked {
                            pid: s.pid,
                            identity,
                        });
                        self.cur_child = Some(s.child);
                        self.cur_ipc = s.ipc.map(IpcChannel::new);
                        self.keep_log_tasks(s.log_tasks);
                        self.queue.push_back(Event::Spawned);
                    }
                    Err(e) => {
                        self.cur_fork = None;
                        self.spawn_error(e.to_string());
                        self.queue.push_back(Event::SpawnFailed);
                    }
                }
            }
            Effect::SignalStop => self.stop_current(),
            Effect::ForceKill => self.signal_current(Signal::KILL),
            Effect::ArmTimer {
                timer,
                after,
                generation,
            } => {
                self.timer = Some((timer, generation, Instant::now() + after));
            }
            Effect::SpawnReplacement => self.spawn_replacement(),
            Effect::PromoteReplacement => self.promote(),
            Effect::AbandonReplacement => self.abandon(),
        }
    }

    fn spawn_replacement(&mut self) {
        if let Some((c, _)) = &self.cluster {
            let wid = c.spawn(self.index);
            self.rep_worker = Some(Worker {
                wid,
                pid: None,
                identity: None,
            });
            return;
        }
        match process::spawn(&self.spec, self.index, &self.out_log, &self.err_log) {
            Ok(s) => {
                let identity = Identity::of(s.pid).ok();
                self.managed.insert(s.pid, identity);
                self.rep_fork = Some(Forked {
                    pid: s.pid,
                    identity,
                });
                self.rep_child = Some(s.child);
                self.rep_ipc = s.ipc.map(IpcChannel::new);
                self.keep_log_tasks(s.log_tasks);
                self.queue.push_back(Event::ReplacementSpawned);
            }
            Err(e) => {
                self.spawn_error(e.to_string());
                self.queue.push_back(Event::ReplacementSpawnFailed);
            }
        }
    }

    /// The replacement becomes current; the old current process retires.
    fn promote(&mut self) {
        if self.cluster.is_some() {
            if let Some(old) = self.cur_worker.take() {
                self.retire_worker(old.wid);
            }
            self.cur_worker = self.rep_worker.take();
        } else {
            let old_ipc = self.cur_ipc.take();
            if let (Some(child), Some(old)) = (self.cur_child.take(), self.cur_fork.take()) {
                self.retire_child(child, old.pid, old_ipc);
            }
            self.cur_ipc = self.rep_ipc.take();
            self.cur_child = self.rep_child.take();
            self.cur_fork = self.rep_fork.take();
        }
        self.out.emit(&ShimEvent::Reload {
            app: &self.spec.name,
            instance: self.index,
            outcome: "succeeded",
        });
    }

    /// The reload failed or was interrupted: stop the replacement, if any.
    fn abandon(&mut self) {
        if let Some(w) = self.rep_worker.take() {
            if w.pid.is_some() {
                self.retire_worker(w.wid);
            } else {
                self.abandoned.insert(w.wid);
            }
        }
        let rep_ipc = self.rep_ipc.take();
        if let (Some(child), Some(f)) = (self.rep_child.take(), self.rep_fork.take()) {
            self.retire_child(child, f.pid, rep_ipc);
        }
        self.out.emit(&ShimEvent::Reload {
            app: &self.spec.name,
            instance: self.index,
            outcome: "failed",
        });
    }

    /// Stop a forked process gracefully in the background: the stop signal
    /// (or the `'shutdown'` message), then SIGKILL after `kill_timeout`,
    /// then clear its process group.
    fn retire_child(
        &mut self,
        mut child: tokio::process::Child,
        pid: u32,
        ipc: Option<IpcChannel>,
    ) {
        let messaged =
            self.spec.shutdown_with_message && ipc.as_ref().is_some_and(IpcChannel::send_shutdown);
        let treekill = self.spec.treekill;
        let kill_timeout = self.inst.policy().kill_timeout;
        let stop_signal = self.kill_signal();
        let managed = self.managed.clone();
        let signal = move |sig| {
            let _ = if treekill {
                process::signal_group(pid, sig)
            } else {
                process::signal_process(pid, sig)
            };
        };
        self.retiring.push(tokio::spawn(async move {
            // Keep the channel open while the app shuts down.
            let _ipc = ipc;
            if !messaged {
                signal(stop_signal);
            }
            if tokio::time::timeout(kill_timeout, child.wait())
                .await
                .is_err()
            {
                signal(Signal::KILL);
                let _ = child.wait().await;
            }
            managed.remove(pid);
            if treekill && process::group_alive(pid) {
                let _ = process::signal_group(pid, Signal::KILL);
            }
        }));
    }

    /// Stop a cluster worker gracefully in the background: no new
    /// connections, in-flight ones finish, then SIGTERM; SIGKILL after
    /// `kill_timeout`.
    fn retire_worker(&mut self, wid: u32) {
        let Some((c, _)) = &self.cluster else {
            return;
        };
        let c = c.clone();
        let kill_timeout = self.inst.policy().kill_timeout;
        let stop_signal = self.kill_signal();
        let message = self.spec.shutdown_with_message;
        self.retiring.push(tokio::spawn(async move {
            c.retire(wid, stop_signal, message);
            tokio::time::sleep(kill_timeout).await;
            // Ignored by the primary if the worker is already gone.
            c.signal(wid, Signal::KILL);
        }));
    }

    async fn finish_retiring(&mut self) {
        let tasks = std::mem::take(&mut self.retiring);
        let limit = self.inst.policy().kill_timeout + KILL_CONFIRM_TIMEOUT;
        let all = async {
            for t in tasks {
                let _ = t.await;
            }
        };
        let _ = tokio::time::timeout(limit, all).await;
    }

    /// The configured graceful-stop signal (SIGINT by default, as PM2).
    fn kill_signal(&self) -> Signal {
        match self.spec.policy.kill_signal {
            vexor_core::KillSignal::Int => Signal::INT,
            vexor_core::KillSignal::Term => Signal::TERM,
            vexor_core::KillSignal::Hup => Signal::HUP,
            vexor_core::KillSignal::Quit => Signal::QUIT,
            vexor_core::KillSignal::Usr1 => Signal::USR1,
            vexor_core::KillSignal::Usr2 => Signal::USR2,
        }
    }

    fn keep_log_tasks(&mut self, tasks: Vec<JoinHandle<()>>) {
        self.log_tasks.retain(|t| !t.is_finished());
        self.log_tasks.extend(tasks);
    }

    fn spawn_error(&self, error: String) {
        self.out.emit(&ShimEvent::SpawnError {
            app: &self.spec.name,
            instance: self.index,
            exec: &self.spec.exec,
            error,
        });
    }

    /// Graceful stop of the current process: the `'shutdown'` message when
    /// configured and possible, else the stop signal.
    fn stop_current(&self) {
        if self.spec.shutdown_with_message {
            if let Some((c, _)) = &self.cluster {
                if let Some(w) = self.cur_worker.filter(|w| w.pid.is_some()) {
                    c.message_shutdown(w.wid);
                }
                return;
            }
            if self.cur_ipc.as_ref().is_some_and(IpcChannel::send_shutdown) {
                return;
            }
        }
        self.signal_current(self.kill_signal());
    }

    fn signal_current(&self, sig: Signal) {
        if let Some((c, _)) = &self.cluster {
            // Workers share the primary's process group, so a worker is
            // signalled on its own, through the primary.
            if let Some(w) = self.cur_worker.filter(|w| w.pid.is_some()) {
                c.signal(w.wid, sig);
            }
            return;
        }
        // Only while our child is unreaped: its PID (and group) can't have
        // been reused yet.
        let Some(pid) = self
            .cur_fork
            .map(|f| f.pid)
            .filter(|_| self.cur_child.is_some())
        else {
            return;
        };
        let sent = if self.spec.treekill {
            process::signal_group(pid, sig)
        } else {
            process::signal_process(pid, sig)
        };
        if let Err(e) = sent {
            eprintln!(
                "vexor shim: {}[{}]: signal {sig:?} to group {pid} failed: {e}",
                self.spec.name, self.index
            );
        }
    }

    fn emit_state(&self) {
        let running = self.alive();
        self.out.emit(&ShimEvent::State {
            app: &self.spec.name,
            instance: self.index,
            state: self.inst.state(),
            pid: self.current_pid().filter(|_| running),
            start: self.current_identity().filter(|_| running).map(|i| i.start),
            restarts: self.inst.restarts(),
            exit: self.inst.last_exit().filter(|_| !running),
            error: self.inst.error(),
        });
    }

    /// The main process has exited. Make sure nothing is left in its process
    /// group before the exit is reported, so a restart never runs next to
    /// leftovers (holding ports, files, or queue messages).
    ///
    /// When stopping, the group already got SIGTERM and has until the stop
    /// deadline. After a crash, the group gets SIGTERM now and `kill_timeout`.
    /// Then SIGKILL.
    async fn clean_group(&mut self, pid: u32) {
        orphan::reap(&self.managed);
        if !process::group_alive(pid) {
            return;
        }
        let stopping = self.inst.state() == State::Stopping;
        let deadline = match self.timer {
            Some((Timer::Stop, _, at)) if stopping => at,
            _ if stopping => Instant::now(),
            _ => {
                self.emit_cleanup("SIGTERM");
                let _ = process::signal_group(pid, Signal::TERM);
                Instant::now() + self.inst.policy().kill_timeout
            }
        };
        if self.wait_group_gone(pid, deadline).await {
            return;
        }
        self.emit_cleanup("SIGKILL");
        let _ = process::signal_group(pid, Signal::KILL);
        if !self
            .wait_group_gone(pid, Instant::now() + KILL_CONFIRM_TIMEOUT)
            .await
        {
            eprintln!(
                "vexor shim: {}[{}]: processes in group {pid} survived SIGKILL",
                self.spec.name, self.index
            );
        }
    }

    async fn wait_group_gone(&self, pid: u32, deadline: Instant) -> bool {
        loop {
            orphan::reap(&self.managed);
            if !process::group_alive(pid) {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(POLL).await;
        }
    }

    fn emit_cleanup(&self, signal: &'static str) {
        self.out.emit(&ShimEvent::GroupCleanup {
            app: &self.spec.name,
            instance: self.index,
            signal,
        });
    }

    /// Let output already in the pipes reach the log, then stop reading.
    /// A leftover process holding a pipe open must not keep the shim's log
    /// writer alive forever, so readers still running after the timeout are
    /// cancelled.
    async fn drain_logs(&mut self) {
        let tasks = std::mem::take(&mut self.log_tasks);
        let aborts: Vec<_> = tasks.iter().map(JoinHandle::abort_handle).collect();
        let all = async {
            for t in tasks {
                let _ = t.await;
            }
        };
        if tokio::time::timeout(LOG_DRAIN_TIMEOUT, all).await.is_err() {
            for a in aborts {
                a.abort();
            }
        }
    }
}

async fn wait_child(child: &mut Option<tokio::process::Child>) -> ExitStatus {
    let Some(c) = child else {
        return std::future::pending().await;
    };
    match c.wait().await {
        Ok(status) => {
            use std::os::unix::process::ExitStatusExt;
            ExitStatus {
                code: status.code(),
                signal: status.signal(),
            }
        }
        Err(e) => {
            eprintln!("vexor shim: wait failed: {e}");
            ExitStatus::default()
        }
    }
}

async fn recv_probe(health: &mut Option<HealthRun>) -> Option<ProbeResult> {
    match health {
        Some(h) => h.rx.recv().await,
        None => std::future::pending().await,
    }
}

async fn memory_tick(tick: &mut Option<tokio::time::Interval>) {
    match tick {
        Some(t) => {
            t.tick().await;
        }
        None => std::future::pending().await,
    }
}

async fn sleep_until(deadline: Option<Instant>) {
    match deadline {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}

async fn recv_worker(
    cluster: &mut Option<(Cluster, mpsc::UnboundedReceiver<WorkerMsg>)>,
) -> Option<WorkerMsg> {
    match cluster {
        Some((_, rx)) => rx.recv().await,
        None => std::future::pending().await,
    }
}

async fn recv_ipc(ipc: &mut Option<IpcChannel>) -> Option<IpcMessage> {
    match ipc {
        Some(ch) => ch.recv().await,
        None => std::future::pending().await,
    }
}

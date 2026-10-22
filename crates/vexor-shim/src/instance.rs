//! Runs one app instance: feeds real-world events into the core state machine
//! and carries out the effects it returns.

use std::collections::VecDeque;
use std::hash::BuildHasher;
use std::sync::Arc;
use std::time::Duration;

use rustix::process::Signal;
use tokio::sync::{broadcast, mpsc, watch};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use vexor_core::{AppSpec, Ctx, Effect, Event, ExitStatus, Instance, Millis, State, Timer};

use crate::cluster::{Cluster, WorkerEvent};
use crate::identity::Identity;
use crate::logs::Sink;
use crate::orphan::{self, Managed};
use crate::output::{Output, ShimEvent};
use crate::process::{self, IpcChannel, IpcMessage};

/// How long to wait for log pipes to drain after the instance has finished.
/// A grandchild that inherited stdout can hold a pipe open indefinitely.
const LOG_DRAIN_TIMEOUT: Duration = Duration::from_secs(2);
/// After SIGKILL, how long to wait for the group to disappear.
const KILL_CONFIRM_TIMEOUT: Duration = Duration::from_secs(2);
const POLL: Duration = Duration::from_millis(25);

pub struct Runner {
    spec: Arc<AppSpec>,
    index: u32,
    inst: Instance,
    out: Output,
    epoch: Instant,
    managed: Managed,
    out_log: Sink,
    err_log: Sink,
    child: Option<tokio::process::Child>,
    /// Cluster mode: the shared primary, and this instance's worker events.
    cluster: Option<(Cluster, mpsc::UnboundedReceiver<WorkerEvent>)>,
    /// Cluster mode: a worker is running (between `Spawned` and `Exited`).
    worker_live: bool,
    identity: Option<Identity>,
    pid: Option<u32>,
    ipc: Option<IpcChannel>,
    timer: Option<(Timer, u64, Instant)>,
    log_tasks: Vec<JoinHandle<()>>,
    queue: VecDeque<Event>,
    rand: std::collections::hash_map::RandomState,
    rand_counter: u64,
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
        let inst = Instance::new(spec.policy.to_policy());
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
            child: None,
            cluster,
            worker_live: false,
            identity: None,
            pid: None,
            ipc: None,
            timer: None,
            log_tasks: Vec::new(),
            queue: VecDeque::new(),
            rand: std::collections::hash_map::RandomState::new(),
            rand_counter: 0,
        }
    }

    /// Start the instance and supervise it. Ends once `shutdown` turns true
    /// and the instance has stopped, or, when `exit_when_idle`, as soon as it
    /// is stopped or errored. `restart` delivers operator restarts.
    pub async fn run(
        mut self,
        mut shutdown: watch::Receiver<bool>,
        mut restart: broadcast::Receiver<()>,
        exit_when_idle: bool,
    ) -> State {
        self.queue.push_back(Event::Start);
        let mut shutting_down = false;
        let mut restarts_open = true;
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
            tokio::select! {
                status = wait_child(&mut self.child), if self.child.is_some() => {
                    self.child = None;
                    self.ipc = None;
                    if let Some(pid) = self.pid {
                        self.managed.remove(pid);
                        if self.spec.treekill {
                            self.clean_group(pid).await;
                        }
                    }
                    self.queue.push_back(Event::Exited(status));
                }
                () = sleep_until(deadline), if deadline.is_some() => {
                    if let Some((timer, generation, _)) = self.timer.take() {
                        self.queue.push_back(Event::TimerFired { timer, generation });
                    }
                }
                ev = recv_worker(&mut self.cluster), if self.cluster.is_some() => match ev {
                    Some(ev) => self.on_worker(ev),
                    None => self.cluster = None,
                },
                msg = recv_ipc(&mut self.ipc), if self.ipc.is_some() => match msg {
                    Some(IpcMessage::Ready) => self.queue.push_back(Event::Ready),
                    Some(IpcMessage::Other) => {}
                    None => self.ipc = None,
                },
                changed = shutdown.changed(), if !shutting_down => {
                    if changed.is_err() || *shutdown.borrow() {
                        shutting_down = true;
                        self.queue.push_back(Event::Stop);
                    }
                }
                r = restart.recv(), if restarts_open && !shutting_down => match r {
                    Ok(()) | Err(broadcast::error::RecvError::Lagged(_)) => {
                        self.queue.push_back(Event::Restart);
                    }
                    Err(broadcast::error::RecvError::Closed) => restarts_open = false,
                },
            }
        }
        self.drain_logs().await;
        self.inst.state()
    }

    /// A process for this instance is running.
    fn alive(&self) -> bool {
        self.child.is_some() || self.worker_live
    }

    fn on_worker(&mut self, ev: WorkerEvent) {
        match ev {
            WorkerEvent::Spawned { pid } => {
                self.pid = Some(pid);
                self.identity = Identity::of(pid).ok();
                self.worker_live = true;
                self.queue.push_back(Event::Spawned);
            }
            WorkerEvent::SpawnFailed(error) => {
                self.pid = None;
                self.identity = None;
                self.out.emit(&ShimEvent::SpawnError {
                    app: &self.spec.name,
                    instance: self.index,
                    exec: &self.spec.exec,
                    error,
                });
                self.queue.push_back(Event::SpawnFailed);
            }
            WorkerEvent::Ready => self.queue.push_back(Event::Ready),
            // Used by readiness-gated reload (plan step 4.3).
            WorkerEvent::Listening => {}
            WorkerEvent::Exited(status) => {
                self.worker_live = false;
                self.queue.push_back(Event::Exited(status));
            }
        }
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
        for effect in effects {
            self.apply(effect);
        }
        if self.inst.armed_timer().is_none() {
            self.timer = None;
        }
        if self.inst.state() != before {
            self.emit_state();
        }
    }

    fn apply(&mut self, effect: Effect) {
        match effect {
            Effect::Spawn if self.cluster.is_some() => {
                // The worker's `Spawned`/`SpawnFailed` arrives as an event.
                if let Some((c, _)) = &self.cluster {
                    c.spawn(self.index);
                }
            }
            Effect::Spawn => {
                match process::spawn(&self.spec, self.index, &self.out_log, &self.err_log) {
                    Ok(s) => {
                        self.pid = Some(s.pid);
                        // The process may already have exited; identity is then
                        // unknown, which only matters for recovery.
                        self.identity = Identity::of(s.pid).ok();
                        self.managed.insert(s.pid, self.identity);
                        self.child = Some(s.child);
                        self.ipc = s.ipc.map(IpcChannel::new);
                        self.log_tasks.retain(|t| !t.is_finished());
                        self.log_tasks.extend(s.log_tasks);
                        self.queue.push_back(Event::Spawned);
                    }
                    Err(e) => {
                        self.pid = None;
                        self.identity = None;
                        self.out.emit(&ShimEvent::SpawnError {
                            app: &self.spec.name,
                            instance: self.index,
                            exec: &self.spec.exec,
                            error: e.to_string(),
                        });
                        self.queue.push_back(Event::SpawnFailed);
                    }
                }
            }
            Effect::SignalStop => self.signal(Signal::TERM),
            Effect::ForceKill => self.signal(Signal::KILL),
            Effect::ArmTimer {
                timer,
                after,
                generation,
            } => {
                self.timer = Some((timer, generation, Instant::now() + after));
            }
        }
    }

    fn signal(&self, sig: Signal) {
        if let Some((c, _)) = &self.cluster {
            // Workers share the primary's process group, so a worker is
            // signalled on its own, through the primary.
            if self.worker_live {
                c.signal(self.index, sig);
            }
            return;
        }
        // Only while our child is unreaped: its PID (and group) can't have
        // been reused yet.
        let Some(pid) = self.pid.filter(|_| self.child.is_some()) else {
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
            pid: self.pid.filter(|_| running),
            start: self.identity.filter(|_| running).map(|i| i.start),
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

async fn sleep_until(deadline: Option<Instant>) {
    match deadline {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}

async fn recv_worker(
    cluster: &mut Option<(Cluster, mpsc::UnboundedReceiver<WorkerEvent>)>,
) -> Option<WorkerEvent> {
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

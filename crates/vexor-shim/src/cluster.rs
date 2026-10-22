//! Cluster mode (docs/architecture.md §6).
//!
//! One actor task per shim owns the cluster primary: a Node process running
//! `cluster.js`, which forks workers that share listening sockets. Each
//! instance's [`Runner`](crate::instance::Runner) keeps its own lifecycle
//! state machine and drives its worker through this actor: `spawn(id)`,
//! `signal(id, …)`, and a stream of [`WorkerEvent`]s for that id.
//!
//! The primary is started on the first spawn request and again on demand if
//! it dies. When it dies, its process group (all workers) is killed and
//! every live worker is reported as exited, so each runner restarts its
//! worker under its usual backoff and crash budget.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use rustix::process::Signal;
use serde_json::{Value, json};
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::net::unix::OwnedWriteHalf;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use vexor_core::{AppSpec, ExitStatus};

use crate::identity::Identity;
use crate::logs::Sink;
use crate::orphan::Managed;
use crate::process;

const PRIMARY_JS: &str = include_str!("cluster.js");
/// Longest message accepted from the primary.
const MAX_LINE: usize = 64 * 1024;
/// How long the primary gets to exit after `shutdown` before SIGKILL.
const SHUTDOWN_WAIT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkerEvent {
    Spawned {
        pid: u32,
    },
    SpawnFailed(String),
    /// The worker called `process.send('ready')`.
    Ready,
    /// The worker started listening on a socket.
    Listening,
    Exited(ExitStatus),
}

enum Cmd {
    Subscribe(u32, mpsc::UnboundedSender<WorkerEvent>),
    Spawn(u32),
    Signal(u32, Signal),
    Shutdown(oneshot::Sender<()>),
}

/// Cheap handle to the actor.
#[derive(Debug, Clone)]
pub struct Cluster {
    tx: mpsc::UnboundedSender<Cmd>,
}

impl Cluster {
    pub fn start(spec: Arc<AppSpec>, managed: Managed, out: Sink, err: Sink) -> Cluster {
        let (tx, rx) = mpsc::unbounded_channel();
        let actor = Actor {
            spec,
            managed,
            out,
            err,
            primary: None,
            subscribers: BTreeMap::new(),
            live: BTreeSet::new(),
            pending: BTreeSet::new(),
            log_tasks: Vec::new(),
        };
        tokio::spawn(actor.run(rx));
        Cluster { tx }
    }

    /// Events for worker `id` (one subscriber per id).
    pub fn subscribe(&self, id: u32) -> mpsc::UnboundedReceiver<WorkerEvent> {
        let (tx, rx) = mpsc::unbounded_channel();
        let _ = self.tx.send(Cmd::Subscribe(id, tx));
        rx
    }

    pub fn spawn(&self, id: u32) {
        let _ = self.tx.send(Cmd::Spawn(id));
    }

    pub fn signal(&self, id: u32, sig: Signal) {
        let _ = self.tx.send(Cmd::Signal(id, sig));
    }

    /// Stop the primary (its workers must already have been stopped).
    pub async fn shutdown(&self) {
        let (tx, rx) = oneshot::channel();
        if self.tx.send(Cmd::Shutdown(tx)).is_ok() {
            let _ = rx.await;
        }
    }
}

struct Primary {
    child: tokio::process::Child,
    pid: u32,
    writer: OwnedWriteHalf,
    messages: mpsc::UnboundedReceiver<Value>,
    reader: JoinHandle<()>,
}

struct Actor {
    spec: Arc<AppSpec>,
    managed: Managed,
    out: Sink,
    err: Sink,
    primary: Option<Primary>,
    subscribers: BTreeMap<u32, mpsc::UnboundedSender<WorkerEvent>>,
    /// Workers the primary reported as spawned and not yet exited.
    live: BTreeSet<u32>,
    /// Spawn requests sent but not yet answered.
    pending: BTreeSet<u32>,
    log_tasks: Vec<JoinHandle<()>>,
}

impl Actor {
    async fn run(mut self, mut rx: mpsc::UnboundedReceiver<Cmd>) {
        loop {
            tokio::select! {
                cmd = rx.recv() => match cmd {
                    Some(Cmd::Subscribe(id, tx)) => { self.subscribers.insert(id, tx); }
                    Some(Cmd::Spawn(id)) => self.spawn(id).await,
                    Some(Cmd::Signal(id, sig)) => self.signal(id, sig).await,
                    Some(Cmd::Shutdown(done)) => {
                        self.shutdown().await;
                        let _ = done.send(());
                        return;
                    }
                    None => { self.shutdown().await; return; }
                },
                msg = next_message(&mut self.primary) => match msg {
                    Some(m) => self.on_message(&m),
                    // Channel closed: the primary is exiting.
                    None => self.primary_gone().await,
                },
            }
        }
    }

    fn tell(&self, id: u32, ev: WorkerEvent) {
        if let Some(tx) = self.subscribers.get(&id) {
            let _ = tx.send(ev);
        }
    }

    async fn spawn(&mut self, id: u32) {
        if self.primary.is_none()
            && let Err(e) = self.start_primary()
        {
            self.tell(
                id,
                WorkerEvent::SpawnFailed(format!("cannot start the cluster primary: {e}")),
            );
            return;
        }
        self.pending.insert(id);
        if !self.send(&json!({ "cmd": "spawn", "id": id })).await {
            self.primary_gone().await;
        }
    }

    async fn signal(&mut self, id: u32, sig: Signal) {
        if !self.live.contains(&id) {
            return;
        }
        let name = if sig == Signal::KILL {
            "SIGKILL"
        } else {
            "SIGTERM"
        };
        if !self
            .send(&json!({ "cmd": "signal", "id": id, "signal": name }))
            .await
        {
            self.primary_gone().await;
        }
    }

    fn start_primary(&mut self) -> std::io::Result<()> {
        let cluster = self
            .spec
            .cluster
            .as_ref()
            .ok_or_else(|| std::io::Error::other("not a cluster app"))?;
        let config = json!({
            "script": cluster.script,
            "args": cluster.args,
            "node_args": cluster.node_args,
            "instance_var": self.spec.instance_var,
        });
        let args = vec!["-e".to_owned(), PRIMARY_JS.to_owned(), config.to_string()];
        let s = process::spawn_with(&self.spec, &args, None, true, &self.out, &self.err)?;
        self.managed.insert(s.pid, Identity::of(s.pid).ok());
        self.log_tasks.retain(|t| !t.is_finished());
        self.log_tasks.extend(s.log_tasks);
        let stream = s
            .ipc
            .ok_or_else(|| std::io::Error::other("no IPC channel to the primary"))?;
        let (read, writer) = stream.into_split();
        let (tx, messages) = mpsc::unbounded_channel();
        let reader = tokio::spawn(async move {
            let mut r = BufReader::new(read);
            let mut buf = Vec::new();
            loop {
                buf.clear();
                match vexor_proto::read_bounded_line(&mut r, &mut buf, MAX_LINE).await {
                    Ok(vexor_proto::LineRead::Line) => {
                        if let Ok(v) = serde_json::from_slice::<Value>(&buf)
                            && tx.send(v).is_err()
                        {
                            return;
                        }
                    }
                    Ok(vexor_proto::LineRead::TooLong) => {}
                    Ok(vexor_proto::LineRead::Eof) | Err(_) => return,
                }
            }
        });
        self.primary = Some(Primary {
            child: s.child,
            pid: s.pid,
            writer,
            messages,
            reader,
        });
        Ok(())
    }

    async fn send(&mut self, msg: &Value) -> bool {
        let Some(p) = self.primary.as_mut() else {
            return false;
        };
        let mut line = msg.to_string();
        line.push('\n');
        p.writer.write_all(line.as_bytes()).await.is_ok()
    }

    fn on_message(&mut self, m: &Value) {
        // Node also sends its own internal messages; only `ev` ones are ours.
        let Some(ev) = m.get("ev").and_then(Value::as_str) else {
            return;
        };
        let Some(id) = m
            .get("id")
            .and_then(Value::as_u64)
            .and_then(|i| u32::try_from(i).ok())
        else {
            return;
        };
        match ev {
            "spawned" => {
                self.pending.remove(&id);
                if let Some(pid) = m
                    .get("pid")
                    .and_then(Value::as_u64)
                    .and_then(|p| u32::try_from(p).ok())
                {
                    self.live.insert(id);
                    self.tell(id, WorkerEvent::Spawned { pid });
                }
            }
            "spawn_failed" => {
                self.pending.remove(&id);
                let e = m
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown error");
                self.tell(id, WorkerEvent::SpawnFailed(e.to_owned()));
            }
            "ready" => self.tell(id, WorkerEvent::Ready),
            "listening" => self.tell(id, WorkerEvent::Listening),
            "exit" => {
                self.live.remove(&id);
                let code = m
                    .get("code")
                    .and_then(Value::as_i64)
                    .and_then(|c| i32::try_from(c).ok());
                let signal = m
                    .get("signal")
                    .and_then(Value::as_str)
                    .and_then(signal_number);
                self.tell(id, WorkerEvent::Exited(ExitStatus { code, signal }));
            }
            _ => {}
        }
    }

    /// The primary exited (or its channel broke). Kill whatever is left of
    /// its process group, then report every worker as gone so each runner
    /// restarts its own under its policy.
    async fn primary_gone(&mut self) {
        let Some(mut p) = self.primary.take() else {
            return;
        };
        p.reader.abort();
        let _ = process::signal_group(p.pid, Signal::KILL);
        let _ = p.child.wait().await;
        self.managed.remove(p.pid);
        let killed = ExitStatus {
            code: None,
            signal: Some(9),
        };
        for id in std::mem::take(&mut self.live) {
            self.tell(id, WorkerEvent::Exited(killed));
        }
        for id in std::mem::take(&mut self.pending) {
            self.tell(
                id,
                WorkerEvent::SpawnFailed("the cluster primary exited".into()),
            );
        }
    }

    async fn shutdown(&mut self) {
        if self.primary.is_some() {
            let _ = self.send(&json!({ "cmd": "shutdown" })).await;
            if let Some(p) = self.primary.as_mut() {
                let _ = tokio::time::timeout(SHUTDOWN_WAIT, p.child.wait()).await;
            }
            self.primary_gone().await;
        }
        // Let output already in the pipes reach the logs, then stop reading.
        let tasks = std::mem::take(&mut self.log_tasks);
        let aborts: Vec<_> = tasks.iter().map(JoinHandle::abort_handle).collect();
        let all = async {
            for t in tasks {
                let _ = t.await;
            }
        };
        if tokio::time::timeout(Duration::from_secs(2), all)
            .await
            .is_err()
        {
            for a in aborts {
                a.abort();
            }
        }
    }
}

async fn next_message(primary: &mut Option<Primary>) -> Option<Value> {
    match primary {
        Some(p) => p.messages.recv().await,
        None => std::future::pending().await,
    }
}

fn signal_number(name: &str) -> Option<i32> {
    Some(match name {
        "SIGHUP" => 1,
        "SIGINT" => 2,
        "SIGQUIT" => 3,
        "SIGABRT" => 6,
        "SIGKILL" => 9,
        "SIGSEGV" => 11,
        "SIGPIPE" => 13,
        "SIGTERM" => 15,
        _ => return None,
    })
}

//! Lifecycle events.
//!
//! Every event gets a sequence number and goes into a bounded buffer, from
//! which the daemon connection streams it, and which it replays after a
//! reconnect (docs/architecture.md §7). In foreground mode each event is also
//! printed as a JSON line on stdout.

use std::collections::{BTreeMap, VecDeque};
use std::io::Write;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use serde::Serialize;
use tokio::sync::Notify;
use vexor_core::{ErrorReason, ExitStatus, State};
use vexor_proto::HealthStatus;

/// Events kept for replay. Older ones are dropped, and a reconnecting daemon
/// that missed them is told so (`FromShim::Gap`).
const BUFFER_CAPACITY: usize = 10_000;

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ShimEvent<'a> {
    State {
        app: &'a str,
        instance: u32,
        state: State,
        #[serde(skip_serializing_if = "Option::is_none")]
        pid: Option<u32>,
        #[serde(skip_serializing_if = "Option::is_none")]
        start: Option<u64>,
        restarts: u64,
        #[serde(skip_serializing_if = "Option::is_none")]
        exit: Option<ExitStatus>,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<ErrorReason>,
    },
    /// Processes were still in the instance's group after its main process
    /// exited, and were sent `signal`.
    GroupCleanup {
        app: &'a str,
        instance: u32,
        signal: &'static str,
    },
    /// A descendant escaped its process group and was adopted by the shim
    /// (Linux). It is stopped when the shim shuts down.
    Orphan { app: &'a str, pid: u32, start: u64 },
    /// Output was discarded because it arrived faster than it could be
    /// written (`overflow: drop`).
    LogDropped {
        app: &'a str,
        path: &'a Path,
        bytes: u64,
    },
    /// Writing a log file failed (reported once until a write succeeds).
    LogError {
        app: &'a str,
        path: &'a Path,
        error: String,
    },
    /// A reload of `instance` finished: `succeeded` (replacement promoted)
    /// or `failed` (replacement abandoned, previous process kept).
    Reload {
        app: &'a str,
        instance: u32,
        outcome: &'static str,
    },
    /// The instance's resident memory was above `max_memory_bytes`; it is
    /// being restarted.
    MemoryRestart {
        app: &'a str,
        instance: u32,
        pid: u32,
        rss_bytes: u64,
        limit_bytes: u64,
    },
    /// A health check's result changed (passing ↔ failing), or, in cluster
    /// mode, failures triggered a reload (`action`).
    Health {
        app: &'a str,
        #[serde(skip_serializing_if = "Option::is_none")]
        instance: Option<u32>,
        ok: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        action: Option<&'static str>,
    },
    /// The app now runs `instances` instances (`vexor scale`).
    Scaled { app: &'a str, instances: u32 },
    /// A scale-down stopped this instance; it is no longer part of the app.
    InstanceRemoved { app: &'a str, instance: u32 },
    /// A watched file changed: every instance is being restarted.
    WatchRestart { app: &'a str, path: &'a Path },
    /// `cron_restart` fired: every instance is being restarted.
    CronRestart { app: &'a str, scheduled: String },
    SpawnError {
        app: &'a str,
        instance: u32,
        exec: &'a Path,
        error: String,
    },
}

#[derive(Debug, Clone)]
pub struct Output(Arc<Hub>);

#[derive(Debug)]
struct Hub {
    inner: Mutex<Inner>,
    notify: Notify,
    echo_stdout: bool,
}

#[derive(Debug, Default)]
struct Inner {
    next_seq: u64,
    buffer: VecDeque<(u64, serde_json::Value)>,
    /// Latest `state` event per instance.
    latest: BTreeMap<u32, serde_json::Value>,
    /// Latest health result per instance (`None`: app-wide, cluster mode).
    health: BTreeMap<Option<u32>, HealthStatus>,
}

/// Events after a given point, as returned by [`Output::since`].
#[derive(Debug, Default, PartialEq)]
pub struct Batch {
    /// `(from, to)`: events in this range were dropped and can't be replayed.
    pub gap: Option<(u64, u64)>,
    pub events: Vec<(u64, serde_json::Value)>,
    /// Pass this as `cursor` next time.
    pub next: u64,
}

impl Output {
    pub fn new(echo_stdout: bool) -> Self {
        Output(Arc::new(Hub {
            inner: Mutex::new(Inner::default()),
            notify: Notify::new(),
            echo_stdout,
        }))
    }

    pub fn emit(&self, event: &ShimEvent<'_>) {
        let Ok(mut value) = serde_json::to_value(event) else {
            return;
        };
        // When it happened, not when the daemon got it (it may be replayed
        // after a daemon restart).
        let ts = now_ms();
        if let serde_json::Value::Object(m) = &mut value {
            m.insert("ts_ms".into(), serde_json::json!(ts));
        }
        if self.0.echo_stdout {
            // A closed stdout must not stop supervision.
            let mut out = std::io::stdout().lock();
            let _ = writeln!(out, "{value}");
            let _ = out.flush();
        }
        {
            let mut inner = self.lock();
            let seq = inner.next_seq;
            inner.next_seq += 1;
            match event {
                ShimEvent::State {
                    instance, state, ..
                } => {
                    // A new process has no health result yet.
                    if *state == State::Starting {
                        inner.health.remove(&Some(*instance));
                    }
                    inner.latest.insert(*instance, value.clone());
                }
                ShimEvent::Health {
                    instance,
                    ok,
                    detail,
                    action: None,
                    ..
                } => {
                    let status = HealthStatus {
                        ok: *ok,
                        detail: detail.clone(),
                        since_ms: ts,
                    };
                    inner.health.insert(*instance, status);
                }
                _ => {}
            }
            if inner.buffer.len() == BUFFER_CAPACITY {
                inner.buffer.pop_front();
            }
            inner.buffer.push_back((seq, value));
        }
        self.0.notify.notify_waiters();
    }

    /// Sequence number the next event will get.
    pub fn next_seq(&self) -> u64 {
        self.lock().next_seq
    }

    pub fn snapshot(&self) -> Vec<serde_json::Value> {
        self.lock().latest.values().cloned().collect()
    }

    /// Drop what is kept about an instance a scale-down removed.
    pub fn forget_instance(&self, instance: u32) {
        let mut inner = self.lock();
        inner.latest.remove(&instance);
        inner.health.remove(&Some(instance));
    }

    /// Latest health result of `instance`, else the app-wide one.
    pub fn health(&self, instance: u32) -> Option<HealthStatus> {
        let inner = self.lock();
        inner
            .health
            .get(&Some(instance))
            .or_else(|| inner.health.get(&None))
            .cloned()
    }

    /// Buffered events with sequence number `>= cursor`.
    pub fn since(&self, cursor: u64) -> Batch {
        let inner = self.lock();
        let oldest = inner.buffer.front().map_or(inner.next_seq, |(s, _)| *s);
        let gap = (cursor < oldest).then(|| (cursor, oldest - 1));
        let events = inner
            .buffer
            .iter()
            .filter(|(s, _)| *s >= cursor)
            .cloned()
            .collect();
        Batch {
            gap,
            events,
            next: inner.next_seq.max(cursor),
        }
    }

    /// Resolves after the next `emit`. Create it *before* calling
    /// [`Output::since`] so no event is missed in between.
    pub fn changed(&self) -> tokio::sync::futures::Notified<'_> {
        self.0.notify.notified()
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.0.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Milliseconds since the epoch.
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn orphan(out: &Output, pid: u32) {
        out.emit(&ShimEvent::Orphan {
            app: "a",
            pid,
            start: 0,
        });
    }

    #[test]
    fn replays_from_cursor() {
        let out = Output::new(false);
        for pid in 0..5 {
            orphan(&out, pid);
        }
        let b = out.since(3);
        assert_eq!(b.gap, None);
        assert_eq!(b.events.iter().map(|e| e.0).collect::<Vec<_>>(), [3, 4]);
        assert_eq!(b.next, 5);
        assert_eq!(out.since(5).events, vec![]);
    }

    #[test]
    fn reports_gap_when_buffer_overflowed() {
        let out = Output::new(false);
        let total = u32::try_from(BUFFER_CAPACITY).expect("fits") + 10;
        for pid in 0..total {
            orphan(&out, pid);
        }
        let b = out.since(0);
        assert_eq!(b.gap, Some((0, 9)));
        assert_eq!(b.events.len(), BUFFER_CAPACITY);
        assert_eq!(b.events[0].0, 10);
    }

    #[test]
    fn snapshot_keeps_latest_state_per_instance() {
        let out = Output::new(false);
        for (instance, state) in [
            (0, State::Starting),
            (1, State::Starting),
            (0, State::Online),
        ] {
            out.emit(&ShimEvent::State {
                app: "a",
                instance,
                state,
                pid: None,
                start: None,
                restarts: 0,
                exit: None,
                error: None,
            });
        }
        let snap = out.snapshot();
        assert_eq!(snap.len(), 2);
        assert_eq!(snap[0]["state"], "online");
        assert_eq!(snap[1]["state"], "starting");
    }

    #[test]
    fn health_is_kept_per_instance_until_a_new_process_starts() {
        let out = Output::new(false);
        let health = |instance, ok| ShimEvent::Health {
            app: "a",
            instance,
            ok,
            detail: None,
            action: None,
        };
        out.emit(&health(Some(0), false));
        out.emit(&health(None, true));
        assert!(!out.health(0).expect("own").ok);
        assert!(out.health(1).expect("app-wide").ok);
        out.emit(&ShimEvent::State {
            app: "a",
            instance: 0,
            state: State::Starting,
            pid: None,
            start: None,
            restarts: 1,
            exit: None,
            error: None,
        });
        assert!(out.health(0).expect("app-wide again").ok);
    }
}

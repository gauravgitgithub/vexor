//! Event history: `$VEXOR_HOME/events.db` (SQLite, WAL).
//!
//! - One writer thread owns the write connection and is fed through a bounded
//!   queue, so lifecycle code never waits on SQLite. When the queue is full,
//!   events are counted as dropped and a `events_dropped` row records how
//!   many, once the writer catches up.
//! - Each event from a shim is stored together with the shim's resume cursor
//!   (`shim_start`, `last_seq`) in one transaction, so a restarted daemon
//!   asks the shim to replay exactly what was not stored yet.
//! - A corrupt file is renamed `events.db.corrupt-<ts>` (never deleted) and a
//!   fresh one is created. If no database can be opened at all, the daemon
//!   keeps supervising without history and reports that it is degraded.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OpenFlags, params};
use serde_json::{Value, json};
use vexor_proto::Resume;

pub const FILE: &str = "events.db";
const SCHEMA_VERSION: i32 = 1;
const QUEUE: usize = 10_000;
/// Default retention.
pub const MAX_AGE: Duration = Duration::from_secs(30 * 24 * 3600);
pub const MAX_ROWS: i64 = 1_000_000;
const PRUNE_EVERY: Duration = Duration::from_secs(3600);

enum Write {
    Shim {
        app: String,
        shim_start: u64,
        seq: u64,
        event: Value,
    },
    Daemon {
        app: Option<String>,
        kind: String,
        data: Value,
    },
    Prune,
}

/// Handle used by the rest of the daemon. Cheap to clone.
#[derive(Debug, Clone)]
pub struct Events {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    path: PathBuf,
    tx: Option<SyncSender<Write>>,
    dropped: AtomicU64,
    /// Human-readable problems to show to the operator (`vexor list`).
    warnings: Mutex<Vec<String>>,
}

impl Events {
    /// Open (or create, or recover) the database and start the writer.
    pub fn open(home: &Path) -> Events {
        let path = home.join(FILE);
        let mut warnings = Vec::new();
        let conn = match open_checked(&path) {
            Ok(c) => Some(c),
            Err(first) => {
                // Keep the bad file for inspection, start a fresh one.
                let kept = path.with_file_name(format!("{FILE}.corrupt-{}", now_ms()));
                let moved = std::fs::rename(&path, &kept).is_ok();
                for ext in ["-wal", "-shm"] {
                    let side = PathBuf::from(format!("{}{ext}", path.display()));
                    let _ = std::fs::rename(&side, format!("{}{ext}", kept.display()));
                }
                match open_checked(&path) {
                    Ok(c) => {
                        warnings.push(format!(
                            "event history was unreadable ({first}) and has been reset{}",
                            if moved {
                                format!("; the old file is kept at {}", kept.display())
                            } else {
                                String::new()
                            }
                        ));
                        Some(c)
                    }
                    Err(second) => {
                        warnings.push(format!(
                            "event history is unavailable ({second}); supervision continues without it"
                        ));
                        None
                    }
                }
            }
        };
        let tx = conn.map(|c| {
            let (tx, rx) = sync_channel(QUEUE);
            std::thread::Builder::new()
                .name("vexor-events".into())
                .spawn(move || writer(c, &rx))
                .map(|_| tx)
        });
        let tx = match tx {
            Some(Ok(tx)) => Some(tx),
            Some(Err(e)) => {
                warnings.push(format!("event history writer could not start: {e}"));
                None
            }
            None => None,
        };
        let events = Events {
            inner: Arc::new(Inner {
                path,
                tx,
                dropped: AtomicU64::new(0),
                warnings: Mutex::new(warnings),
            }),
        };
        if events.inner.tx.is_some() {
            let handle = events.clone();
            std::thread::Builder::new()
                .name("vexor-events-prune".into())
                .spawn(move || {
                    loop {
                        handle.send(Write::Prune);
                        std::thread::sleep(PRUNE_EVERY);
                    }
                })
                .ok();
        }
        events
    }

    pub fn warnings(&self) -> Vec<String> {
        self.inner
            .warnings
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    pub fn record_shim(&self, app: &str, shim_start: u64, seq: u64, event: Value) {
        self.send(Write::Shim {
            app: app.to_owned(),
            shim_start,
            seq,
            event,
        });
    }

    pub fn record_daemon(&self, app: Option<&str>, kind: &str, data: Value) {
        self.send(Write::Daemon {
            app: app.map(str::to_owned),
            kind: kind.to_owned(),
            data,
        });
    }

    fn send(&self, w: Write) {
        let Some(tx) = &self.inner.tx else {
            return;
        };
        match tx.try_send(w) {
            Ok(()) => {
                let dropped = self.inner.dropped.swap(0, Ordering::Relaxed);
                if dropped > 0 {
                    let note = Write::Daemon {
                        app: None,
                        kind: "events_dropped".into(),
                        data: json!({ "count": dropped }),
                    };
                    if tx.try_send(note).is_err() {
                        self.inner.dropped.fetch_add(dropped, Ordering::Relaxed);
                    }
                }
            }
            Err(TrySendError::Full(_)) => {
                self.inner.dropped.fetch_add(1, Ordering::Relaxed);
            }
            Err(TrySendError::Disconnected(_)) => {}
        }
    }

    /// Last stored position for `app`'s shim, to resume from.
    pub fn resume(&self, app: &str) -> Option<Resume> {
        let conn = self.reader().ok()?;
        conn.query_row(
            "SELECT shim_start, last_seq FROM cursors WHERE app = ?1",
            params![app],
            |r| {
                Ok(Resume {
                    shim_start: u64::try_from(r.get::<_, i64>(0)?).unwrap_or(0),
                    last_seq: u64::try_from(r.get::<_, i64>(1)?).unwrap_or(0),
                })
            },
        )
        .ok()
    }

    /// Newest-last list of events, filtered.
    pub fn query(
        &self,
        app: Option<&str>,
        since_ms: Option<u64>,
        limit: u32,
    ) -> Result<Vec<Value>, String> {
        let conn = self.reader().map_err(|e| format!("event history: {e}"))?;
        let since = since_ms.map_or(0, |s| i64::try_from(s).unwrap_or(i64::MAX));
        let mut stmt = conn
            .prepare(
                "SELECT data, shim_start, seq FROM (
                   SELECT id, data, shim_start, seq FROM events
                   WHERE (?1 IS NULL OR app = ?1) AND ts_ms >= ?2
                   ORDER BY ts_ms DESC, id DESC LIMIT ?3
                 ) ORDER BY id ASC",
            )
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map(params![app, since, i64::from(limit)], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, Option<i64>>(1)?,
                    r.get::<_, Option<i64>>(2)?,
                ))
            })
            .map_err(|e| e.to_string())?;
        let mut out = Vec::new();
        for row in rows {
            let (text, shim_start, seq) = row.map_err(|e| e.to_string())?;
            let mut event = serde_json::from_str(&text).unwrap_or(Value::String(text));
            // Position in the shim's stream, for gap and duplicate checks.
            if let (Value::Object(m), Some(start), Some(seq)) = (&mut event, shim_start, seq) {
                m.insert("shim_start".into(), json!(start));
                m.insert("seq".into(), json!(seq));
            }
            out.push(event);
        }
        Ok(out)
    }

    fn reader(&self) -> rusqlite::Result<Connection> {
        if self.inner.tx.is_none() {
            return Err(rusqlite::Error::InvalidPath(self.inner.path.clone()));
        }
        let c = Connection::open_with_flags(
            &self.inner.path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        c.busy_timeout(Duration::from_secs(2))?;
        Ok(c)
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

/// Open, check integrity, apply the schema. Any failure means "unusable".
fn open_checked(path: &Path) -> Result<Connection, String> {
    let conn = Connection::open(path).map_err(|e| e.to_string())?;
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    conn.busy_timeout(Duration::from_secs(2))
        .map_err(|e| e.to_string())?;
    let check: String = conn
        .query_row("PRAGMA quick_check", [], |r| r.get(0))
        .map_err(|e| e.to_string())?;
    if check != "ok" {
        return Err(format!("integrity check failed: {check}"));
    }
    conn.pragma_update(None, "journal_mode", "WAL")
        .map_err(|e| e.to_string())?;
    conn.pragma_update(None, "synchronous", "NORMAL")
        .map_err(|e| e.to_string())?;
    let version: i32 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .map_err(|e| e.to_string())?;
    match version {
        0 => {
            conn.execute_batch(
                "BEGIN;
                 CREATE TABLE events (
                   id INTEGER PRIMARY KEY,
                   ts_ms INTEGER NOT NULL,
                   app TEXT,
                   shim_start INTEGER,
                   seq INTEGER,
                   kind TEXT NOT NULL,
                   data TEXT NOT NULL,
                   UNIQUE (app, shim_start, seq)
                 );
                 CREATE INDEX events_app_ts ON events (app, ts_ms);
                 CREATE INDEX events_ts ON events (ts_ms);
                 CREATE TABLE cursors (
                   app TEXT PRIMARY KEY,
                   shim_start INTEGER NOT NULL,
                   last_seq INTEGER NOT NULL
                 );
                 PRAGMA user_version = 1;
                 COMMIT;",
            )
            .map_err(|e| e.to_string())?;
        }
        SCHEMA_VERSION => {}
        v => {
            return Err(format!(
                "schema version {v} is newer than this vexor supports ({SCHEMA_VERSION})"
            ));
        }
    }
    Ok(conn)
}

fn writer(mut conn: Connection, rx: &Receiver<Write>) {
    // Batch whatever is queued into one transaction per round.
    while let Ok(first) = rx.recv() {
        let mut batch = vec![first];
        while let Ok(more) = rx.try_recv() {
            batch.push(more);
            if batch.len() >= 1000 {
                break;
            }
        }
        if let Err(e) = write_batch(&mut conn, batch) {
            eprintln!("vexor daemon: event history write failed: {e}");
        }
    }
}

fn write_batch(conn: &mut Connection, batch: Vec<Write>) -> rusqlite::Result<()> {
    let tx = conn.transaction()?;
    for w in batch {
        match w {
            Write::Shim {
                app,
                shim_start,
                seq,
                event,
            } => {
                let ts = event["ts_ms"].as_i64().unwrap_or_else(now_ms);
                let kind = event["type"].as_str().unwrap_or("event").to_owned();
                let start = i64::try_from(shim_start).unwrap_or(i64::MAX);
                let seq = i64::try_from(seq).unwrap_or(i64::MAX);
                tx.execute(
                    "INSERT OR IGNORE INTO events (ts_ms, app, shim_start, seq, kind, data)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![ts, app, start, seq, kind, event.to_string()],
                )?;
                tx.execute(
                    "INSERT INTO cursors (app, shim_start, last_seq) VALUES (?1, ?2, ?3)
                     ON CONFLICT (app) DO UPDATE SET
                       last_seq = CASE WHEN shim_start = excluded.shim_start
                                       THEN max(last_seq, excluded.last_seq)
                                       ELSE excluded.last_seq END,
                       shim_start = excluded.shim_start",
                    params![app, start, seq],
                )?;
            }
            Write::Daemon { app, kind, data } => {
                let mut data = data;
                if let Value::Object(m) = &mut data {
                    m.insert("type".into(), json!(kind));
                    if let Some(a) = &app {
                        m.insert("app".into(), json!(a));
                    }
                    m.insert("ts_ms".into(), json!(now_ms()));
                }
                tx.execute(
                    "INSERT INTO events (ts_ms, app, kind, data) VALUES (?1, ?2, ?3, ?4)",
                    params![now_ms(), app, kind, data.to_string()],
                )?;
            }
            Write::Prune => {
                let cutoff = now_ms() - i64::try_from(MAX_AGE.as_millis()).unwrap_or(i64::MAX);
                tx.execute("DELETE FROM events WHERE ts_ms < ?1", params![cutoff])?;
                tx.execute(
                    "DELETE FROM events WHERE id <= (SELECT id FROM events ORDER BY id DESC LIMIT 1 OFFSET ?1)",
                    params![MAX_ROWS],
                )?;
            }
        }
    }
    tx.commit()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wait_for(events: &Events, n: usize) -> Vec<Value> {
        for _ in 0..200 {
            let got = events.query(None, None, 1000).expect("query");
            if got.len() >= n {
                return got;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("events never arrived");
    }

    /// Recent timestamps: the writer prunes anything older than MAX_AGE.
    fn base() -> u64 {
        u64::try_from(now_ms()).expect("positive")
    }

    fn state(ts: u64) -> Value {
        json!({ "type": "state", "instance": 0, "state": "online", "ts_ms": ts })
    }

    #[test]
    fn stores_events_and_resume_cursor_together() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ev = Events::open(dir.path());
        let t = base();
        for seq in 0..5 {
            ev.record_shim("web", 77, seq, state(t + seq));
        }
        ev.record_shim("web", 77, 3, state(t + 3)); // replayed duplicate
        let got = wait_for(&ev, 5);
        assert_eq!(got.len(), 5, "duplicates must be ignored");
        assert_eq!(
            ev.resume("web"),
            Some(Resume {
                shim_start: 77,
                last_seq: 4
            })
        );
        // A new shim incarnation replaces the cursor.
        ev.record_shim("web", 99, 0, state(t + 9));
        wait_for(&ev, 6);
        assert_eq!(
            ev.resume("web"),
            Some(Resume {
                shim_start: 99,
                last_seq: 0
            })
        );
        assert!(ev.warnings().is_empty());
    }

    #[test]
    fn query_filters_by_app_time_and_limit() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ev = Events::open(dir.path());
        let t = base();
        for seq in 0..10 {
            ev.record_shim(if seq % 2 == 0 { "a" } else { "b" }, 1, seq, state(t + seq));
        }
        wait_for(&ev, 10);
        assert_eq!(ev.query(Some("a"), None, 100).expect("q").len(), 5);
        let since = ev.query(None, Some(t + 7), 100).expect("q");
        assert_eq!(since.len(), 3);
        let last2 = ev.query(None, None, 2).expect("q");
        assert_eq!(
            last2
                .iter()
                .map(|e| e["ts_ms"].as_u64())
                .collect::<Vec<_>>(),
            [Some(t + 8), Some(t + 9)],
            "newest events, oldest first"
        );
    }

    #[test]
    fn corrupt_file_is_kept_and_replaced() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(FILE);
        std::fs::write(&path, vec![0x42; 8192]).expect("write garbage");
        let ev = Events::open(dir.path());
        let warnings = ev.warnings();
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("reset"), "{warnings:?}");
        let kept: Vec<_> = std::fs::read_dir(dir.path())
            .expect("dir")
            .flatten()
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with("events.db.corrupt-")
            })
            .collect();
        assert_eq!(kept.len(), 1);
        assert_eq!(
            std::fs::read(kept[0].path()).expect("read"),
            vec![0x42; 8192]
        );
        ev.record_daemon(None, "daemon_started", json!({}));
        assert_eq!(wait_for(&ev, 1).len(), 1);
    }

    #[test]
    fn unusable_location_degrades_without_failing() {
        if rustix::process::geteuid().is_root() {
            eprintln!("skipping: permissions don't restrict root");
            return;
        }
        let dir = tempfile::tempdir().expect("tempdir");
        // A directory where the file should be: can't be opened or replaced
        // by rename-then-create, since the directory itself is read-only.
        let home = dir.path().join("ro");
        std::fs::create_dir(&home).expect("mkdir");
        std::fs::create_dir(home.join(FILE)).expect("blocker");
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o500)).expect("chmod");
        }
        let ev = Events::open(&home);
        ev.record_daemon(None, "x", json!({}));
        assert!(ev.query(None, None, 10).is_err());
        assert!(ev.resume("x").is_none());
        let warnings = ev.warnings();
        assert!(
            warnings.iter().any(|w| w.contains("unavailable")),
            "{warnings:?}"
        );
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o700)).expect("chmod");
        }
    }

    #[test]
    fn prune_drops_old_rows() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ev = Events::open(dir.path());
        // 1970: far older than MAX_AGE. The writer also prunes at startup,
        // so the old row may go before or after our explicit prune.
        ev.record_shim("old", 1, 0, state(1_000));
        ev.record_daemon(None, "recent", json!({}));
        ev.send(Write::Prune);
        for _ in 0..200 {
            let kinds: Vec<Value> = ev
                .query(None, None, 10)
                .expect("q")
                .into_iter()
                .map(|e| e["type"].clone())
                .collect();
            if kinds == [json!("recent")] {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("old row was not pruned");
    }
}

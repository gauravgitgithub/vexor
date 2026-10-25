//! Event history: `$VEXOR_HOME/events.db` (SQLite, WAL).
//!
//! - One writer thread owns the write connection and is fed through a bounded
//!   queue, so lifecycle code never waits on SQLite. When the queue is full,
//!   events are counted as dropped and a `events_dropped` row records how
//!   many, once the writer catches up.
//! - Each event from a shim is stored together with the shim's resume cursor
//!   (`shim_start`, `last_seq`) in one transaction, so a restarted daemon
//!   asks the shim to replay exactly what was not stored yet.
//! - Resource samples (plan 6b.2) go to `metrics_raw`, and each also
//!   updates its 1-minute and 1-hour buckets in `metrics_rollup` (sums,
//!   maxima and a count), so averages need no separate rollup pass. Raw
//!   samples are kept for a day, minutes for a week, hours for 90 days.
//!   Compact on purpose (integer app ids, CPU in tenths of a percent,
//!   memory in KiB, rollup times in seconds): see docs/plan.md 6b.2 for
//!   measured sizes.
//! - Per-route request stats (plan 6b.5) go straight into 1-minute and
//!   1-hour buckets in `routes_rollup` (counts, errors, total and longest
//!   duration, a sparse latency histogram), keyed by `route_names`.
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
use vexor_proto::{
    InstanceMetrics, MetricsPoint, Resume, RoutePoint, RouteSummary, RouteWindow, hist,
};

pub const FILE: &str = "events.db";
const SCHEMA_VERSION: i32 = 4;
const QUEUE: usize = 10_000;
/// Default retention.
pub const MAX_AGE: Duration = Duration::from_secs(30 * 24 * 3600);
pub const MAX_ROWS: i64 = 1_000_000;
const PRUNE_EVERY: Duration = Duration::from_secs(3600);

/// Raw samples kept for.
pub const RAW_KEEP: Duration = Duration::from_secs(24 * 3600);
/// Rollups: (id, bucket width in seconds, kept for).
pub const ROLLUPS: [(i64, i64, Duration); 2] = [
    (1, 60, Duration::from_secs(7 * 24 * 3600)),
    (2, 3600, Duration::from_secs(90 * 24 * 3600)),
];
/// Raw rows kept at most (100 processes sampled every 5 s for a day is
/// 1.7M). Above it, raw samples are kept for proportionally less than a
/// day, so a host with many more processes stays bounded.
pub const MAX_RAW_METRICS: i64 = 3_000_000;
/// Longest a resource sample waits in memory before it is written.
const METRICS_FLUSH: Duration = Duration::from_secs(2);
/// Distinct routes stored per app; requests to others are stored as
/// `(other)`, so a route that slips past normalization can't grow the
/// database without bound.
pub const MAX_ROUTES_PER_APP: i64 = 1_000;
/// Routes with a time series in a `routes` answer (the busiest).
pub const ROUTE_SERIES: usize = 20;

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
    Metrics {
        app: String,
        samples: Vec<InstanceMetrics>,
    },
    Routes {
        app: String,
        window: RouteWindow,
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

    /// Store one round of resource samples. Dropped like events when the
    /// writer is behind.
    pub fn record_metrics(&self, app: &str, samples: Vec<InstanceMetrics>) {
        if !samples.is_empty() {
            self.send(Write::Metrics {
                app: app.to_owned(),
                samples,
            });
        }
    }

    /// Store one window of per-route request stats.
    pub fn record_routes(&self, app: &str, window: RouteWindow) {
        if !window.routes.is_empty() {
            self.send(Write::Routes {
                app: app.to_owned(),
                window,
            });
        }
    }

    /// Per-route stats of `app` between `since_ms` and `until_ms`, in
    /// per-minute buckets, or per-hour ones for long or old ranges: a
    /// summary per route (busiest first) and a series for the busiest
    /// [`ROUTE_SERIES`]. Returns the bucket width in ms.
    pub fn routes(
        &self,
        app: &str,
        since_ms: u64,
        until_ms: u64,
    ) -> Result<(i64, Vec<RouteSummary>, Vec<RoutePoint>), String> {
        let conn = self.reader().map_err(|e| format!("route history: {e}"))?;
        let since = i64::try_from(since_ms / 1000).unwrap_or(i64::MAX);
        let until = i64::try_from(until_ms / 1000).unwrap_or(i64::MAX);
        let span = until.saturating_sub(since);
        let oldest = |keep: Duration| now_ms() / 1000 - i64::try_from(keep.as_secs()).unwrap_or(0);
        let (res, width, _) = ROLLUPS
            .iter()
            .copied()
            .find(|(_, width, keep)| span / width <= 1_500 && since >= oldest(*keep))
            .unwrap_or(ROLLUPS[1]);
        let mut stmt = conn
            .prepare(
                "SELECT n.method, n.route, r.ts_s, r.n, r.client_errors, r.server_errors,
                        r.total_ms, r.max_ms, r.hist
                 FROM routes_rollup r
                 JOIN route_names n ON n.id = r.route
                 JOIN metric_apps a ON a.id = n.app
                 WHERE a.name = ?1 AND r.res = ?2 AND r.ts_s >= ?3 AND r.ts_s <= ?4
                 ORDER BY n.method, n.route, r.ts_s",
            )
            .map_err(|e| e.to_string())?;
        struct Bucket {
            ts_s: i64,
            n: u64,
            server_errors: u64,
            hist: Vec<(u16, u64)>,
        }
        struct Route {
            summary: RouteSummary,
            total_ms: f64,
            hist: Vec<(u16, u64)>,
            buckets: Vec<Bucket>,
        }
        let mut routes: Vec<Route> = Vec::new();
        let u = |v: i64| u64::try_from(v).unwrap_or(0);
        let rows = stmt
            .query_map(
                params![app, res, since - since.rem_euclid(width), until],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, i64>(2)?,
                        u(r.get(3)?),
                        u(r.get(4)?),
                        u(r.get(5)?),
                        r.get::<_, f64>(6)?,
                        r.get::<_, f64>(7)?,
                        hist::decode(&r.get::<_, Vec<u8>>(8)?),
                    ))
                },
            )
            .map_err(|e| e.to_string())?;
        for row in rows {
            let (method, route, ts_s, n, client, server, total_ms, max_ms, h) =
                row.map_err(|e| e.to_string())?;
            let same = routes
                .last()
                .is_some_and(|r| r.summary.method == method && r.summary.route == route);
            if !same {
                routes.push(Route {
                    summary: RouteSummary {
                        method,
                        route,
                        count: 0,
                        client_errors: 0,
                        server_errors: 0,
                        avg_ms: 0.0,
                        p50_ms: 0.0,
                        p95_ms: 0.0,
                        p99_ms: 0.0,
                        max_ms: 0.0,
                    },
                    total_ms: 0.0,
                    hist: Vec::new(),
                    buckets: Vec::new(),
                });
            }
            let r = routes.last_mut().expect("pushed");
            r.summary.count += n;
            r.summary.client_errors += client;
            r.summary.server_errors += server;
            r.summary.max_ms = r.summary.max_ms.max(max_ms);
            r.total_ms += total_ms;
            hist::merge(&mut r.hist, &h);
            r.buckets.push(Bucket {
                ts_s,
                n,
                server_errors: server,
                hist: h,
            });
        }
        routes.sort_by(|a, b| {
            b.summary
                .count
                .cmp(&a.summary.count)
                .then_with(|| a.summary.route.cmp(&b.summary.route))
        });
        let mut summaries = Vec::with_capacity(routes.len());
        let mut points = Vec::new();
        for (i, mut r) in routes.into_iter().enumerate() {
            let q = |q| round1(hist::quantile(&r.hist, q).unwrap_or(0.0));
            #[allow(clippy::cast_precision_loss)]
            let avg = r.total_ms / r.summary.count.max(1) as f64;
            r.summary.avg_ms = round1(avg);
            r.summary.p50_ms = q(0.5);
            r.summary.p95_ms = q(0.95);
            r.summary.p99_ms = q(0.99);
            r.summary.max_ms = round1(r.summary.max_ms);
            if i < ROUTE_SERIES {
                let index = u32::try_from(i).unwrap_or(u32::MAX);
                points.extend(r.buckets.iter().map(|b| RoutePoint {
                    route: index,
                    ts_ms: u(b.ts_s) * 1000,
                    count: b.n,
                    server_errors: b.server_errors,
                    p95_ms: round1(hist::quantile(&b.hist, 0.95).unwrap_or(0.0)),
                }));
            }
            summaries.push(r.summary);
        }
        Ok((width * 1000, summaries, points))
    }

    /// Resource history of `app` between `since_ms` and `until_ms`: raw
    /// samples when the range is short and still kept, else per-minute or
    /// per-hour buckets, keeping the series to about 1,500 points. Returns
    /// the point width in ms (5,000 assumed for raw samples).
    pub fn metrics(
        &self,
        app: &str,
        since_ms: u64,
        until_ms: u64,
    ) -> Result<(i64, Vec<MetricsPoint>), String> {
        let conn = self.reader().map_err(|e| format!("metrics history: {e}"))?;
        let since = i64::try_from(since_ms / 1000).unwrap_or(i64::MAX);
        let until = i64::try_from(until_ms / 1000).unwrap_or(i64::MAX);
        let span = until.saturating_sub(since);
        let kept_since =
            |keep: Duration| now_ms() / 1000 - i64::try_from(keep.as_secs()).unwrap_or(0);
        let u = |v: i64| u64::try_from(v).unwrap_or(0);
        let cpu = |tenths: i64| tenths as f64 / 10.0;
        if span / 5 <= 1_500 && since >= kept_since(RAW_KEEP) {
            let mut stmt = conn
                .prepare(
                    "SELECT r.instance, r.ts_ms, r.pid, r.cpu, r.rss_kib, r.fds
                     FROM metrics_raw r JOIN metric_apps a ON a.id = r.app
                     WHERE a.name = ?1 AND r.ts_ms >= ?2 AND r.ts_ms <= ?3
                     ORDER BY r.instance, r.ts_ms",
                )
                .map_err(|e| e.to_string())?;
            let rows = stmt
                .query_map(params![app, since * 1000, until * 1000 + 999], |r| {
                    let c: Option<i64> = r.get(3)?;
                    let rss = u(r.get(4)?) * 1024;
                    Ok(MetricsPoint {
                        instance: u32::try_from(r.get::<_, i64>(0)?).unwrap_or(0),
                        ts_ms: u(r.get(1)?),
                        pid: u32::try_from(r.get::<_, i64>(2)?).unwrap_or(0),
                        samples: 1,
                        cpu_avg: c.map(cpu),
                        cpu_max: c.map(cpu),
                        rss_avg: rss,
                        rss_max: rss,
                        fds_max: r.get::<_, Option<i64>>(5)?.map(u),
                    })
                })
                .map_err(|e| e.to_string())?;
            let points = rows
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| e.to_string())?;
            return Ok((5_000, points));
        }
        let (res, width, _) = ROLLUPS
            .iter()
            .copied()
            .find(|(_, width, keep)| span / width <= 1_500 && since >= kept_since(*keep))
            .unwrap_or(ROLLUPS[1]);
        let mut stmt = conn
            .prepare(
                "SELECT r.instance, r.ts_s, r.pid, r.n, r.cpu_n, r.cpu_sum, r.cpu_max,
                        r.rss_sum, r.rss_max, r.fds_max
                 FROM metrics_rollup r JOIN metric_apps a ON a.id = r.app
                 WHERE a.name = ?1 AND r.res = ?2 AND r.ts_s >= ?3 AND r.ts_s <= ?4
                 ORDER BY r.instance, r.ts_s",
            )
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map(
                params![app, res, since - since.rem_euclid(width), until],
                |r| {
                    let n: i64 = r.get(3)?;
                    let cpu_n: i64 = r.get(4)?;
                    let cpu_sum: i64 = r.get(5)?;
                    let rss_sum: i64 = r.get(7)?;
                    Ok(MetricsPoint {
                        instance: u32::try_from(r.get::<_, i64>(0)?).unwrap_or(0),
                        ts_ms: u(r.get(1)?) * 1000,
                        pid: u32::try_from(r.get::<_, i64>(2)?).unwrap_or(0),
                        samples: u(n),
                        cpu_avg: (cpu_n > 0).then(|| round1(cpu_sum as f64 / cpu_n as f64 / 10.0)),
                        cpu_max: r.get::<_, Option<i64>>(6)?.map(cpu),
                        rss_avg: u(rss_sum / n.max(1)) * 1024,
                        rss_max: u(r.get(8)?) * 1024,
                        fds_max: r.get::<_, Option<i64>>(9)?.map(u),
                    })
                },
            )
            .map_err(|e| e.to_string())?;
        let points = rows
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?;
        Ok((width * 1000, points))
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
        self.query_kind(app, None, since_ms, limit)
    }

    /// Like [`Events::query`], only events of type `kind` if given.
    pub fn query_kind(
        &self,
        app: Option<&str>,
        kind: Option<&str>,
        since_ms: Option<u64>,
        limit: u32,
    ) -> Result<Vec<Value>, String> {
        let conn = self.reader().map_err(|e| format!("event history: {e}"))?;
        let since = since_ms.map_or(0, |s| i64::try_from(s).unwrap_or(i64::MAX));
        let mut stmt = conn
            .prepare(
                "SELECT data, shim_start, seq FROM (
                   SELECT id, ts_ms, data, shim_start, seq FROM events
                   WHERE (?1 IS NULL OR app = ?1) AND ts_ms >= ?2 AND (?4 IS NULL OR kind = ?4)
                   ORDER BY ts_ms DESC, id DESC LIMIT ?3
                 ) ORDER BY ts_ms ASC, id ASC",
            )
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map(params![app, since, i64::from(limit), kind], |r| {
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

fn round1(v: f64) -> f64 {
    (v * 10.0).round() / 10.0
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
            migrate_to_2(&conn)?;
            migrate_to_3(&conn)?;
            migrate_to_4(&conn)?;
        }
        1 => {
            migrate_to_2(&conn)?;
            migrate_to_3(&conn)?;
            migrate_to_4(&conn)?;
        }
        2 => {
            migrate_to_3(&conn)?;
            migrate_to_4(&conn)?;
        }
        3 => migrate_to_4(&conn)?,
        SCHEMA_VERSION => {}
        v => {
            return Err(format!(
                "schema version {v} is newer than this vexor supports ({SCHEMA_VERSION})"
            ));
        }
    }
    Ok(conn)
}

/// Schema 2: resource history (see the module docs).
fn migrate_to_2(conn: &Connection) -> Result<(), String> {
    conn.execute_batch(
        "BEGIN;
         CREATE TABLE metric_apps (id INTEGER PRIMARY KEY, name TEXT NOT NULL UNIQUE);
         CREATE TABLE metrics_raw (
           app INTEGER NOT NULL,
           instance INTEGER NOT NULL,
           ts_s INTEGER NOT NULL,
           pid INTEGER NOT NULL,
           cpu INTEGER,
           rss_kib INTEGER NOT NULL,
           fds INTEGER,
           PRIMARY KEY (app, instance, ts_s)
         ) WITHOUT ROWID;
         CREATE TABLE metrics_rollup (
           app INTEGER NOT NULL,
           res INTEGER NOT NULL,
           instance INTEGER NOT NULL,
           ts_s INTEGER NOT NULL,
           pid INTEGER NOT NULL,
           n INTEGER NOT NULL,
           cpu_n INTEGER NOT NULL,
           cpu_sum INTEGER NOT NULL,
           cpu_max INTEGER,
           rss_sum INTEGER NOT NULL,
           rss_max INTEGER NOT NULL,
           fds_max INTEGER,
           PRIMARY KEY (app, res, instance, ts_s)
         ) WITHOUT ROWID;
         PRAGMA user_version = 2;
         COMMIT;",
    )
    .map_err(|e| e.to_string())
}

/// Schema 3: raw samples keyed by the millisecond. With second keys, a
/// replacement process sampled in the same second as the one that just
/// died overwrote its last sample.
fn migrate_to_3(conn: &Connection) -> Result<(), String> {
    conn.execute_batch(
        "BEGIN;
         ALTER TABLE metrics_raw RENAME TO metrics_raw_v2;
         CREATE TABLE metrics_raw (
           app INTEGER NOT NULL,
           instance INTEGER NOT NULL,
           ts_ms INTEGER NOT NULL,
           pid INTEGER NOT NULL,
           cpu INTEGER,
           rss_kib INTEGER NOT NULL,
           fds INTEGER,
           PRIMARY KEY (app, instance, ts_ms)
         ) WITHOUT ROWID;
         INSERT INTO metrics_raw
           SELECT app, instance, ts_s * 1000, pid, cpu, rss_kib, fds FROM metrics_raw_v2;
         DROP TABLE metrics_raw_v2;
         PRAGMA user_version = 3;
         COMMIT;",
    )
    .map_err(|e| e.to_string())
}

/// Schema 4: per-route request stats (see the module docs).
fn migrate_to_4(conn: &Connection) -> Result<(), String> {
    conn.execute_batch(
        "BEGIN;
         CREATE TABLE route_names (
           id INTEGER PRIMARY KEY,
           app INTEGER NOT NULL,
           method TEXT NOT NULL,
           route TEXT NOT NULL,
           UNIQUE (app, method, route)
         );
         CREATE TABLE routes_rollup (
           route INTEGER NOT NULL,
           res INTEGER NOT NULL,
           ts_s INTEGER NOT NULL,
           n INTEGER NOT NULL,
           client_errors INTEGER NOT NULL,
           server_errors INTEGER NOT NULL,
           total_ms REAL NOT NULL,
           max_ms REAL NOT NULL,
           hist BLOB NOT NULL,
           PRIMARY KEY (route, res, ts_s)
         ) WITHOUT ROWID;
         PRAGMA user_version = 4;
         COMMIT;",
    )
    .map_err(|e| e.to_string())
}

fn writer(mut conn: Connection, rx: &Receiver<Write>) {
    use std::sync::mpsc::RecvTimeoutError;
    // Resource samples arrive many times a second with 100 apps: they wait
    // up to METRICS_FLUSH and go into one transaction, not one each.
    let mut metrics = Vec::new();
    let mut flush_at: Option<std::time::Instant> = None;
    loop {
        let first = match flush_at {
            Some(at) => {
                match rx.recv_timeout(at.saturating_duration_since(std::time::Instant::now())) {
                    Ok(w) => Some(w),
                    Err(RecvTimeoutError::Timeout) => None,
                    Err(RecvTimeoutError::Disconnected) => {
                        let _ = write_batch(&mut conn, metrics);
                        return;
                    }
                }
            }
            None => match rx.recv() {
                Ok(w) => Some(w),
                Err(_) => return,
            },
        };
        // Batch whatever else is queued into one transaction per round.
        let mut batch = Vec::new();
        for w in first
            .into_iter()
            .chain(std::iter::from_fn(|| rx.try_recv().ok()).take(999))
        {
            if matches!(w, Write::Metrics { .. } | Write::Routes { .. }) {
                metrics.push(w);
                flush_at.get_or_insert_with(|| std::time::Instant::now() + METRICS_FLUSH);
            } else {
                batch.push(w);
            }
        }
        // Before a prune too, so it sees every sample received before it.
        let prune = batch.iter().any(|w| matches!(w, Write::Prune));
        if prune
            || metrics.len() >= 1000
            || flush_at.is_some_and(|at| std::time::Instant::now() >= at)
        {
            metrics.append(&mut batch);
            batch = std::mem::take(&mut metrics);
            flush_at = None;
        }
        if !batch.is_empty()
            && let Err(e) = write_batch(&mut conn, batch)
        {
            eprintln!("vexor daemon: event history write failed: {e}");
        }
    }
}

/// Drop expired metrics, through each (app, instance) key prefix so no
/// table scan is needed.
fn prune_metrics(tx: &rusqlite::Transaction<'_>) -> rusqlite::Result<()> {
    let now = now_ms() / 1000;
    let raw_rows: i64 = tx.query_row("SELECT count(*) FROM metrics_raw", [], |r| r.get(0))?;
    let mut raw_keep = i64::try_from(RAW_KEEP.as_secs()).unwrap_or(i64::MAX);
    if raw_rows > MAX_RAW_METRICS {
        raw_keep = raw_keep * MAX_RAW_METRICS / raw_rows;
    }
    let keys: Vec<(i64, i64)> = tx
        .prepare("SELECT DISTINCT app, instance FROM metrics_rollup WHERE res = 2")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<Result<_, _>>()?;
    for (app, instance) in keys {
        tx.execute(
            "DELETE FROM metrics_raw WHERE app = ?1 AND instance = ?2 AND ts_ms < ?3",
            params![app, instance, (now - raw_keep) * 1000],
        )?;
        for (res, _, keep) in ROLLUPS {
            let keep = i64::try_from(keep.as_secs()).unwrap_or(i64::MAX);
            tx.execute(
                "DELETE FROM metrics_rollup WHERE app = ?1 AND res = ?2 AND instance = ?3 AND ts_s < ?4",
                params![app, res, instance, now - keep],
            )?;
        }
    }
    let routes: Vec<i64> = tx
        .prepare("SELECT id FROM route_names")?
        .query_map([], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    for route in routes {
        for (res, _, keep) in ROLLUPS {
            let keep = i64::try_from(keep.as_secs()).unwrap_or(i64::MAX);
            tx.execute(
                "DELETE FROM routes_rollup WHERE route = ?1 AND res = ?2 AND ts_s < ?3",
                params![route, res, now - keep],
            )?;
        }
    }
    tx.execute(
        "DELETE FROM route_names WHERE id NOT IN (SELECT DISTINCT route FROM routes_rollup)",
        [],
    )?;
    Ok(())
}

fn app_id(tx: &rusqlite::Transaction<'_>, app: &str) -> rusqlite::Result<i64> {
    tx.prepare_cached("INSERT OR IGNORE INTO metric_apps (name) VALUES (?1)")?
        .execute(params![app])?;
    tx.prepare_cached("SELECT id FROM metric_apps WHERE name = ?1")?
        .query_row(params![app], |r| r.get(0))
}

/// The id of a route, created if the app has room for it, else of its
/// `(other)` route.
fn route_id(
    tx: &rusqlite::Transaction<'_>,
    app: i64,
    method: &str,
    route: &str,
) -> rusqlite::Result<i64> {
    // Shims send standard methods only; checked again here.
    let method = vexor_proto::http_method(method);
    let find = |method: &str, route: &str| {
        tx.prepare_cached(
            "SELECT id FROM route_names WHERE app = ?1 AND method = ?2 AND route = ?3",
        )?
        .query_row(params![app, method, route], |r| r.get::<_, i64>(0))
    };
    match find(method, route) {
        Err(rusqlite::Error::QueryReturnedNoRows) => {}
        found => return found,
    }
    let known: i64 = tx
        .prepare_cached("SELECT count(*) FROM route_names WHERE app = ?1")?
        .query_row(params![app], |r| r.get(0))?;
    let (method, route) = if known < MAX_ROUTES_PER_APP {
        (method, route)
    } else {
        ("-", "(other)")
    };
    tx.prepare_cached(
        "INSERT OR IGNORE INTO route_names (app, method, route) VALUES (?1, ?2, ?3)",
    )?
    .execute(params![app, method, route])?;
    find(method, route)
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
            Write::Routes { app, window } => {
                let app = app_id(&tx, &app)?;
                let ts = i64::try_from(window.end_ms.saturating_sub(1) / 1000).unwrap_or(i64::MAX);
                for r in window.routes {
                    let route = route_id(&tx, app, &r.method, &r.route)?;
                    for (res, width, _) in ROLLUPS {
                        let bucket = ts - ts.rem_euclid(width);
                        let mut h: Vec<(u16, u64)> = tx
                            .prepare_cached(
                                "SELECT hist FROM routes_rollup WHERE route = ?1 AND res = ?2 AND ts_s = ?3",
                            )?
                            .query_row(params![route, res, bucket], |row| row.get::<_, Vec<u8>>(0))
                            .map(|b| hist::decode(&b))
                            .or_else(|e| match e {
                                rusqlite::Error::QueryReturnedNoRows => Ok(Vec::new()),
                                e => Err(e),
                            })?;
                        hist::merge(&mut h, &r.hist);
                        let i = |v: u64| i64::try_from(v).unwrap_or(i64::MAX);
                        tx.prepare_cached(
                            "INSERT INTO routes_rollup
                               (route, res, ts_s, n, client_errors, server_errors, total_ms, max_ms, hist)
                             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                             ON CONFLICT (route, res, ts_s) DO UPDATE SET
                               n = n + excluded.n,
                               client_errors = client_errors + excluded.client_errors,
                               server_errors = server_errors + excluded.server_errors,
                               total_ms = total_ms + excluded.total_ms,
                               max_ms = max(max_ms, excluded.max_ms),
                               hist = excluded.hist",
                        )?
                        .execute(params![
                            route,
                            res,
                            bucket,
                            i(r.count),
                            i(r.client_errors),
                            i(r.server_errors),
                            r.total_ms,
                            r.max_ms,
                            hist::encode(&h),
                        ])?;
                    }
                }
            }
            Write::Metrics { app, samples } => {
                let id = app_id(&tx, &app)?;
                let mut raw = tx.prepare_cached(
                    "INSERT OR REPLACE INTO metrics_raw (app, instance, ts_ms, pid, cpu, rss_kib, fds)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                )?;
                let mut rollup = tx.prepare_cached(
                    "INSERT INTO metrics_rollup
                       (app, res, instance, ts_s, pid, n, cpu_n, cpu_sum, cpu_max, rss_sum, rss_max, fds_max)
                     VALUES (?1, ?2, ?3, ?4, ?5, 1, ?6, ?7, ?8, ?9, ?9, ?10)
                     ON CONFLICT (app, res, instance, ts_s) DO UPDATE SET
                       pid = excluded.pid,
                       n = n + 1,
                       cpu_n = cpu_n + excluded.cpu_n,
                       cpu_sum = cpu_sum + excluded.cpu_sum,
                       cpu_max = max(coalesce(cpu_max, excluded.cpu_max), coalesce(excluded.cpu_max, cpu_max)),
                       rss_sum = rss_sum + excluded.rss_sum,
                       rss_max = max(rss_max, excluded.rss_max),
                       fds_max = max(coalesce(fds_max, excluded.fds_max), coalesce(excluded.fds_max, fds_max))",
                )?;
                for m in samples {
                    let ts = i64::try_from(m.ts_ms / 1000).unwrap_or(i64::MAX);
                    #[allow(clippy::cast_possible_truncation)]
                    let cpu = m.cpu_pct.map(|c| (c * 10.0).round() as i64);
                    let rss = i64::try_from(m.rss_bytes.div_ceil(1024)).unwrap_or(i64::MAX);
                    let fds = m.open_fds.map(|f| i64::try_from(f).unwrap_or(i64::MAX));
                    let ts_ms = i64::try_from(m.ts_ms).unwrap_or(i64::MAX);
                    raw.execute(params![id, m.instance, ts_ms, m.pid, cpu, rss, fds])?;
                    for (res, width, _) in ROLLUPS {
                        rollup.execute(params![
                            id,
                            res,
                            m.instance,
                            ts - ts.rem_euclid(width),
                            m.pid,
                            i64::from(cpu.is_some()),
                            cpu.unwrap_or(0),
                            cpu,
                            rss,
                            fds,
                        ])?;
                    }
                }
            }
            Write::Prune => {
                let cutoff = now_ms() - i64::try_from(MAX_AGE.as_millis()).unwrap_or(i64::MAX);
                tx.execute("DELETE FROM events WHERE ts_ms < ?1", params![cutoff])?;
                tx.execute(
                    "DELETE FROM events WHERE id <= (SELECT id FROM events ORDER BY id DESC LIMIT 1 OFFSET ?1)",
                    params![MAX_ROWS],
                )?;
                prune_metrics(&tx)?;
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

    fn sample(instance: u32, pid: u32, ts_ms: u64, cpu: Option<f64>, rss: u64) -> InstanceMetrics {
        InstanceMetrics {
            instance,
            pid,
            ts_ms,
            cpu_pct: cpu,
            rss_bytes: rss,
            open_fds: Some(rss / 100),
            health: None,
        }
    }

    fn wait_points(ev: &Events, since: u64, until: u64, n: usize) -> (i64, Vec<MetricsPoint>) {
        // Samples are written within METRICS_FLUSH.
        for _ in 0..600 {
            let got = ev.metrics("web", since, until).expect("metrics");
            if got.1.len() >= n {
                return got;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("metrics never arrived");
    }

    #[test]
    fn metrics_roll_up_into_minutes_and_hours() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ev = Events::open(dir.path());
        // Start of the current hour, so every sample lands in one hour.
        let now = base();
        let hour = now - now % 3_600_000;
        let t0 = if now - hour > 120_000 {
            hour
        } else {
            hour - 3_600_000
        };
        // Minute 1: cpu 10 and 30; minute 2: first sample has no CPU yet,
        // then 50. A restart in minute 2 (pid 8).
        let rounds = [
            (t0, 7, Some(10.0), 100),
            (t0 + 30_000, 7, Some(30.0), 300),
            (t0 + 60_000, 8, None, 200),
            (t0 + 90_000, 8, Some(50.0), 400),
        ];
        for (ts, pid, cpu, kib) in rounds {
            ev.record_metrics("web", vec![sample(0, pid, ts, cpu, kib * 1024)]);
        }
        let (width, raw) = wait_points(&ev, t0, t0 + 100_000, 4);
        assert_eq!(width, 5_000);
        assert_eq!(raw.len(), 4);
        assert_eq!(raw[2].cpu_avg, None);
        assert_eq!((raw[3].cpu_max, raw[3].rss_max), (Some(50.0), 400 * 1024));

        let conn = ev.reader().expect("reader");
        type Row = (i64, i64, i64, i64, Option<i64>, i64, i64, i64);
        let rows: Vec<Row> = conn
            .prepare(
                "SELECT res, n, cpu_n, cpu_sum, cpu_max, rss_sum, rss_max, pid
                 FROM metrics_rollup ORDER BY res, ts_s",
            )
            .expect("prepare")
            .query_map([], |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                    r.get(7)?,
                ))
            })
            .expect("query")
            .collect::<Result<_, _>>()
            .expect("rows");
        assert_eq!(
            rows,
            [
                // CPU in tenths of a percent, memory in KiB.
                (1, 2, 2, 400, Some(300), 400, 300, 7),
                (1, 2, 1, 500, Some(500), 600, 400, 8),
                (2, 4, 3, 900, Some(500), 1000, 400, 8),
            ]
        );
    }

    #[test]
    fn metrics_resolution_follows_the_range() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ev = Events::open(dir.path());
        let now = base();
        ev.record_metrics("web", vec![sample(0, 1, now, Some(1.0), 1)]);
        wait_points(&ev, now - 1, now + 1, 1);
        let width = |span_ms: u64| ev.metrics("web", now - span_ms, now).expect("q").0;
        assert_eq!(width(3_600_000), 5_000, "1 hour: raw");
        assert_eq!(width(24 * 3_600_000), 60_000, "a day: minutes");
        assert_eq!(width(30 * 24 * 3_600_000), 3_600_000, "a month: hours");
        // Raw samples older than a day are gone: a 2-hour window ending a
        // day and a half ago uses minutes.
        let old = now - 36 * 3_600_000;
        assert_eq!(
            ev.metrics("web", old - 7_200_000, old).expect("q").0,
            60_000
        );
    }

    #[test]
    fn day_of_one_process_queries_fast() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ev = Events::open(dir.path());
        let now = base();
        let start = now - 24 * 3_600_000 + 60_000;
        // A day of 5 s samples (17,280), 500 per message so the bounded
        // queue never drops any.
        let all: Vec<u64> = (0..17_280).collect();
        for chunk in all.chunks(500) {
            let samples = chunk
                .iter()
                .map(|&i| sample(0, 1, start + i * 5_000, Some(1.0), 1 << 20))
                .collect();
            ev.record_metrics("web", samples);
        }
        // Wait until the writer has stored the last sample (slow in debug
        // builds on small CI runners).
        let last = start + 17_279 * 5_000;
        let deadline = std::time::Instant::now() + Duration::from_secs(60);
        while ev.metrics("web", last, last).expect("q").1.is_empty() {
            assert!(
                std::time::Instant::now() < deadline,
                "metrics never arrived"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
        let t = std::time::Instant::now();
        let (width, points) = ev.metrics("web", now - 24 * 3_600_000, now).expect("q");
        let took = t.elapsed();
        assert_eq!(width, 60_000);
        assert!(points.len() >= 1_400, "{}", points.len());
        assert!(took < Duration::from_millis(100), "{took:?}");
    }

    #[test]
    fn schema_1_database_gains_metrics() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(FILE);
        {
            let c = Connection::open(&path).expect("open");
            c.execute_batch(
                "CREATE TABLE events (id INTEGER PRIMARY KEY, ts_ms INTEGER NOT NULL, app TEXT,
                   shim_start INTEGER, seq INTEGER, kind TEXT NOT NULL, data TEXT NOT NULL,
                   UNIQUE (app, shim_start, seq));
                 CREATE TABLE cursors (app TEXT PRIMARY KEY, shim_start INTEGER NOT NULL,
                   last_seq INTEGER NOT NULL);
                 INSERT INTO cursors VALUES ('web', 5, 9);
                 PRAGMA user_version = 1;",
            )
            .expect("v1 schema");
        }
        let ev = Events::open(dir.path());
        assert!(ev.warnings().is_empty(), "{:?}", ev.warnings());
        assert_eq!(ev.resume("web").map(|r| r.last_seq), Some(9), "kept");
        let now = base();
        ev.record_metrics("web", vec![sample(0, 1, now, Some(2.0), 3 << 10)]);
        let window = (now - 1_000, now + 1_000);
        assert_eq!(
            wait_points(&ev, window.0, window.1, 1).1[0].rss_avg,
            3 << 10
        );
    }

    #[test]
    fn a_replacement_sampled_in_the_same_second_keeps_the_last_sample_of_the_old_process() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ev = Events::open(dir.path());
        let t = base() - base() % 1000;
        ev.record_metrics("web", vec![sample(0, 7, t + 100, Some(1.0), 1 << 20)]);
        ev.record_metrics("web", vec![sample(0, 8, t + 300, None, 1 << 20)]);
        let points = wait_points(&ev, t - 1_000, t + 1_000, 2).1;
        assert_eq!(points.iter().map(|p| p.pid).collect::<Vec<_>>(), [7, 8]);
    }

    #[test]
    fn schema_2_raw_samples_move_to_milliseconds() {
        let dir = tempfile::tempdir().expect("tempdir");
        let now = base();
        {
            let c = Connection::open(dir.path().join(FILE)).expect("open");
            c.execute_batch(
                "CREATE TABLE events (id INTEGER PRIMARY KEY, ts_ms INTEGER NOT NULL, app TEXT,
                   shim_start INTEGER, seq INTEGER, kind TEXT NOT NULL, data TEXT NOT NULL,
                   UNIQUE (app, shim_start, seq));
                 CREATE TABLE cursors (app TEXT PRIMARY KEY, shim_start INTEGER NOT NULL,
                   last_seq INTEGER NOT NULL);
                 PRAGMA user_version = 1;",
            )
            .expect("v1");
            migrate_to_2(&c).expect("v2");
            c.execute("INSERT INTO metric_apps (id, name) VALUES (1, 'web')", [])
                .expect("app");
            c.execute(
                "INSERT INTO metrics_raw VALUES (1, 0, ?1, 42, 15, 2048, 9)",
                params![i64::try_from(now / 1000).expect("fits")],
            )
            .expect("row");
        }
        let ev = Events::open(dir.path());
        assert!(ev.warnings().is_empty(), "{:?}", ev.warnings());
        let p = &ev.metrics("web", now - 60_000, now + 1_000).expect("q").1[0];
        assert_eq!(
            (p.pid, p.ts_ms, p.cpu_max, p.rss_max),
            (42, now / 1000 * 1000, Some(1.5), 2 << 20)
        );
    }

    fn window(end_ms: u64, routes: &[(&str, u64, u64, f64)]) -> RouteWindow {
        RouteWindow {
            start_ms: end_ms - 5_000,
            end_ms,
            lines: 10,
            json_lines: 10,
            routes: routes
                .iter()
                .map(
                    |&(route, count, server_errors, ms)| vexor_proto::RouteStats {
                        method: "GET".into(),
                        route: route.into(),
                        count,
                        client_errors: 0,
                        server_errors,
                        #[allow(clippy::cast_precision_loss)]
                        total_ms: ms * count as f64,
                        max_ms: ms,
                        hist: vec![(hist::bucket(ms), count)],
                    },
                )
                .collect(),
        }
    }

    fn wait_routes(
        ev: &Events,
        since: u64,
        until: u64,
        total: u64,
    ) -> (i64, Vec<RouteSummary>, Vec<RoutePoint>) {
        for _ in 0..600 {
            let got = ev.routes("web", since, until).expect("routes");
            if got.1.iter().map(|r| r.count).sum::<u64>() >= total {
                return got;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("routes never arrived");
    }

    #[test]
    fn routes_roll_up_with_latency_quantiles() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ev = Events::open(dir.path());
        let now = base();
        let minute = now - now % 60_000;
        // Two windows in one minute merge; a third lands in the next.
        ev.record_routes(
            "web",
            window(minute + 10_000, &[("/a", 95, 1, 10.0), ("/b", 3, 0, 2.0)]),
        );
        ev.record_routes("web", window(minute + 20_000, &[("/a", 5, 4, 1_000.0)]));
        ev.record_routes("web", window(minute + 70_000, &[("/a", 10, 0, 10.0)]));
        // A window with no requests stores nothing.
        ev.record_routes("web", window(minute + 30_000, &[]));
        let (width, routes, points) = wait_routes(&ev, minute, minute + 120_000, 113);
        assert_eq!(width, 60_000);
        assert_eq!(routes.len(), 2);
        let a = &routes[0];
        assert_eq!((a.route.as_str(), a.count, a.server_errors), ("/a", 110, 5));
        assert!((a.p50_ms - 10.0).abs() < 1.0, "{a:?}");
        assert!((a.p99_ms - 1_000.0).abs() < 100.0, "{a:?}");
        assert!((a.max_ms - 1_000.0).abs() < 1e-9);
        assert!((a.avg_ms - 55.0).abs() < 0.1, "{a:?}");
        let a_points: Vec<_> = points.iter().filter(|p| p.route == 0).collect();
        assert_eq!(a_points.len(), 2);
        assert_eq!((a_points[0].ts_ms, a_points[0].count), (minute, 100));
        assert_eq!(a_points[0].server_errors, 5);
        assert_eq!(
            (a_points[1].ts_ms, a_points[1].count),
            (minute + 60_000, 10)
        );
        // Other apps don't leak in.
        assert!(
            ev.routes("api", minute, minute + 120_000)
                .expect("q")
                .1
                .is_empty()
        );
    }

    #[test]
    fn routes_past_the_per_app_limit_are_stored_as_other() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ev = Events::open(dir.path());
        let now = base();
        let names: Vec<String> = (0..=MAX_ROUTES_PER_APP).map(|i| format!("/r{i}")).collect();
        let routes: Vec<(&str, u64, u64, f64)> =
            names.iter().map(|n| (n.as_str(), 1, 0, 1.0)).collect();
        ev.record_routes("web", window(now, &routes));
        let total = u64::try_from(names.len()).expect("len");
        let (_, got, points) = wait_routes(&ev, now - 60_000, now + 1_000, total);
        assert_eq!(got.len(), names.len());
        assert!(got.iter().any(|r| r.route == "(other)"));
        assert!(
            !got.iter()
                .any(|r| r.route == format!("/r{MAX_ROUTES_PER_APP}"))
        );
        // Series only for the busiest.
        assert!(points.iter().all(|p| (p.route as usize) < ROUTE_SERIES));
    }

    #[test]
    fn prune_drops_expired_routes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ev = Events::open(dir.path());
        let now = base();
        // Eight days ago: the minute row expires, the hour row stays.
        ev.record_routes(
            "web",
            window(now - 8 * 24 * 3_600_000, &[("/old", 1, 0, 1.0)]),
        );
        ev.send(Write::Prune);
        let count = |res: i64| -> i64 {
            ev.reader()
                .expect("reader")
                .query_row(
                    "SELECT count(*) FROM routes_rollup WHERE res = ?1",
                    params![res],
                    |r| r.get(0),
                )
                .expect("count")
        };
        for _ in 0..600 {
            if count(1) == 0 && count(2) == 1 {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("minutes {}, hours {}", count(1), count(2));
    }

    #[test]
    fn prune_drops_expired_metrics_per_resolution() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ev = Events::open(dir.path());
        let now = base();
        // Two days ago: its raw row expires, its minute and hour rows stay.
        let old = now - 48 * 3_600_000;
        ev.record_metrics("web", vec![sample(0, 1, old, Some(1.0), 1)]);
        ev.send(Write::Prune);
        let count = |res: i64| -> i64 {
            let r = ev.reader().expect("reader");
            if res == 0 {
                r.query_row("SELECT count(*) FROM metrics_raw", [], |r| r.get(0))
            } else {
                r.query_row(
                    "SELECT count(*) FROM metrics_rollup WHERE res = ?1",
                    params![res],
                    |r| r.get(0),
                )
            }
            .expect("count")
        };
        for _ in 0..600 {
            if count(0) == 0 && count(1) == 1 && count(2) == 1 {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("raw {}, minutes {}, hours {}", count(0), count(1), count(2));
    }

    /// Storage for 100 processes kept for the full retention, as documented
    /// in docs/plan.md 6b.2. Slow: `cargo test --release -- --ignored`.
    #[test]
    #[ignore]
    fn metrics_storage_for_100_processes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(FILE);
        let mut conn = open_checked(&path).expect("open");
        let now = base();
        let day = 24 * 3_600_000u64;
        // Every 5 s for the last day, every minute for the 6 days before,
        // every hour back to 90 days.
        let mut times: Vec<u64> = (0..day / 5_000).map(|i| now - i * 5_000).collect();
        times.extend((day / 60_000..7 * day / 60_000).map(|i| now - i * 60_000));
        times.extend((7 * day / 3_600_000..90 * day / 3_600_000).map(|i| now - i * 3_600_000));
        times.sort_unstable();
        for chunk in times.chunks(200) {
            let batch = chunk
                .iter()
                .map(|&ts| Write::Metrics {
                    app: "app".into(),
                    samples: (0..100)
                        .map(|i| sample(i, 1000 + i, ts, Some(12.5), 80 << 20))
                        .collect(),
                })
                .collect();
            write_batch(&mut conn, batch).expect("write");
        }
        write_batch(&mut conn, vec![Write::Prune]).expect("prune");
        conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE); VACUUM;")
            .expect("compact");
        let rows: i64 = conn
            .query_row(
                "SELECT (SELECT count(*) FROM metrics_raw) + (SELECT count(*) FROM metrics_rollup)",
                [],
                |r| r.get(0),
            )
            .expect("count");
        let bytes = std::fs::metadata(&path).expect("meta").len();
        println!(
            "rows {rows}, file {:.1} MiB",
            bytes as f64 / f64::from(1 << 20)
        );
    }
}

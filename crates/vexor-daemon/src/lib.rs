//! `vexor daemon`: the control plane (docs/architecture.md §2, §7, §9).
//!
//! The daemon never parents app processes. It owns the desired state
//! (`state.json`), serves CLI requests on `daemon.sock`, and keeps each app's
//! shim in line with its desired state. Killing or restarting the daemon
//! leaves shims and apps untouched; a new daemon reconnects.

mod alerts;
mod events;
mod follower;
mod log;
mod notify;
mod prometheus;
mod state;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex, PoisonError};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::BufReader;
use tokio::net::{UnixListener, UnixStream};
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::{Mutex, mpsc, watch};
use vexor_core::AppSpec;
use vexor_proto::{
    AppStatus, ClientRequest, Desired, ErrorCode, PROTOCOL, Request, Response, Target, ToShim,
};
use vexor_shim::Paths;

use events::Events;
use follower::{Follower, Live};
use log::Log;
use state::{AppEntry, Store};

/// Exit codes of `vexor daemon`.
pub mod exit {
    pub const OK: i32 = 0;
    /// Another daemon already runs for this home directory.
    pub const ALREADY_RUNNING: i32 = 3;
    /// `state.json` is unreadable or from a newer version.
    pub const BAD_STATE: i32 = 5;
    pub const INTERNAL: i32 = 70;
}

/// Name of the CLI socket inside the home directory.
pub const SOCKET: &str = "daemon.sock";
/// Name of the daemon lock inside the home directory.
pub const LOCK: &str = "daemon.lock";
/// `vexor save` snapshot inside the home directory.
pub const DUMP: &str = "dump.json";

const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
/// How long `start` waits for the instances to settle (online, or failed)
/// before answering; they keep starting after that either way.
const START_WAIT: Duration = Duration::from_secs(10);
/// How long `start` waits for a new shim to come up. Generous: on a loaded
/// machine (many apps starting at once) a shim can take several seconds.
const SHIM_WAIT: Duration = Duration::from_secs(60);
/// Extra time `stop` allows on top of the app's kill_timeout.
const STOP_SLACK: Duration = Duration::from_secs(10);
const POLL: Duration = Duration::from_millis(20);

pub fn run(home: &Path) -> i32 {
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("vexor daemon: cannot start runtime: {e}");
            return exit::INTERNAL;
        }
    };
    rt.block_on(run_async(home.to_path_buf()))
}

async fn run_async(home: PathBuf) -> i32 {
    if let Err(e) = vexor_shim::private_dir(&home)
        .and_then(|()| vexor_shim::private_dir(&vexor_shim::shims_dir(&home)))
        .and_then(|()| vexor_shim::private_dir(&home.join("apps")))
    {
        eprintln!("vexor daemon: cannot use {}: {e}", home.display());
        return exit::INTERNAL;
    }
    let _lock = match vexor_shim::lock_with_retry(&home.join(LOCK), vexor_shim::LOCK_WAIT) {
        Ok(Some(lock)) => lock,
        Ok(None) => {
            eprintln!(
                "vexor daemon: another daemon is already running for {}",
                home.display()
            );
            return exit::ALREADY_RUNNING;
        }
        Err(e) => {
            eprintln!("vexor daemon: cannot take daemon lock: {e}");
            return exit::INTERNAL;
        }
    };
    let store = match Store::load(&home.join("state.json")) {
        Ok(s) => s,
        Err(msg) => {
            eprintln!("vexor daemon: {msg}");
            return exit::BAD_STATE;
        }
    };
    let log = match Log::open(&home.join("daemon.log")) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("vexor daemon: cannot open daemon.log: {e}");
            return exit::INTERNAL;
        }
    };
    let (mut term, mut int) = match (
        signal(SignalKind::terminate()),
        signal(SignalKind::interrupt()),
    ) {
        (Ok(t), Ok(i)) => (t, i),
        (Err(e), _) | (_, Err(e)) => {
            eprintln!("vexor daemon: cannot install signal handlers: {e}");
            return exit::INTERNAL;
        }
    };
    // We hold the lock, so any socket file left behind is stale.
    let listener = match vexor_shim::bind_socket(&home.join(SOCKET)) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("vexor daemon: cannot bind {SOCKET}: {e}");
            return exit::INTERNAL;
        }
    };

    let entries: Vec<(String, AppEntry)> = store
        .apps()
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    let events = Events::open(&home);
    for w in events.warnings() {
        log.write(json!({ "type": "warning", "message": w }));
    }
    let daemon = Arc::new(Daemon {
        home: home.clone(),
        log: log.clone(),
        events: events.clone(),
        store: Mutex::new(store),
        apps: StdMutex::new(BTreeMap::new()),
        alerts: StdMutex::new(AlertState::default()),
        notifier: notifier(&events, &log),
        host: notify::hostname(),
        settings_warnings: StdMutex::new(Vec::new()),
    });
    for (name, entry) in entries {
        if let Err(e) = daemon.follow(&name, &entry) {
            log.write(json!({ "type": "error", "app": name, "error": e }));
        }
    }
    log.write(json!({
        "type": "daemon_started",
        "pid": std::process::id(),
        "version": env!("CARGO_PKG_VERSION"),
    }));
    events.record_daemon(None, "daemon_started", json!({ "pid": std::process::id() }));

    let server = tokio::spawn(serve(listener, daemon.clone()));
    let alerting = tokio::spawn(alert_loop(daemon.clone()));
    let exporter = tokio::spawn(prometheus_loop(daemon.clone()));
    // Stopping the daemon leaves shims and apps running.
    tokio::select! {
        _ = term.recv() => {}
        _ = int.recv() => {}
    }
    server.abort();
    alerting.abort();
    exporter.abort();
    let _ = std::fs::remove_file(home.join(SOCKET));
    log.write(json!({ "type": "daemon_stopped" }));
    exit::OK
}

struct Daemon {
    home: PathBuf,
    log: Log,
    events: Events,
    /// Desired state. Held for the whole of every mutating request, which
    /// serializes them (docs/architecture.md §9).
    store: Mutex<Store>,
    apps: StdMutex<BTreeMap<String, Handle>>,
    alerts: StdMutex<AlertState>,
    /// Alert delivery; `None` if its thread could not start.
    notifier: Option<notify::Notifier>,
    host: String,
    /// Problems with settings.json (Prometheus endpoint), for every response.
    settings_warnings: StdMutex<Vec<String>>,
}

/// Alert evaluation (plan 6b.3): rules from `alerts.json`, re-read when it
/// changes; a bad file keeps the previous rules and is reported.
#[derive(Default)]
struct AlertState {
    config: alerts::Config,
    /// Modification time of the file last read; `None` when absent.
    loaded: Option<std::time::SystemTime>,
    error: Option<String>,
    /// Problems with the file's safety (permissions, weakened TLS).
    risks: Vec<String>,
    engine: alerts::Engine,
    restarts: alerts::RestartRate,
    /// Latest size of each app's log files, refreshed every minute.
    log_disk: BTreeMap<String, u64>,
}

impl AlertState {
    fn reload(&mut self, home: &Path, log: &Log) {
        let path = home.join(alerts::FILE);
        let modified = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
        if modified == self.loaded {
            return;
        }
        self.loaded = modified;
        let parsed = match modified {
            None => Ok(alerts::Config::default()),
            Some(_) => std::fs::read_to_string(&path)
                .map_err(|e| e.to_string())
                .and_then(|t| alerts::Config::parse(&t)),
        };
        match parsed {
            Ok(c) => {
                self.risks = c.channels.risks();
                let open_to_others = std::fs::metadata(&path).is_ok_and(|m| {
                    use std::os::unix::fs::PermissionsExt;
                    m.permissions().mode() & 0o077 != 0
                });
                if open_to_others && !c.channels.is_empty() {
                    self.risks.push(format!(
                        "{} configures alert channels and is readable by other users; run chmod 600 on it",
                        path.display()
                    ));
                }
                self.config = c;
                self.error = None;
                log.write(json!({ "type": "alerts_loaded", "file": modified.is_some() }));
            }
            Err(e) => {
                let msg = format!(
                    "{} is invalid, keeping the previous rules: {e}",
                    path.display()
                );
                log.write(json!({ "type": "error", "error": msg }));
                self.error = Some(msg);
            }
        }
    }
}

const ALERT_EVERY: Duration = Duration::from_secs(5);

/// Start the delivery thread; a delivery given up after its retries is
/// recorded as an event.
fn notifier(events: &Events, log: &Log) -> Option<notify::Notifier> {
    let (events, log) = (events.clone(), log.clone());
    let started = notify::Notifier::start(Box::new(move |channel, alert, error| {
        let data = json!({
            "channel": channel,
            "rule": alert.rule,
            "level": alert.level,
            "instance": alert.instance,
            "error": error,
        });
        events.record_daemon(Some(&alert.app), "alert_delivery_failed", data.clone());
        let mut line = data;
        line["type"] = json!("alert_delivery_failed");
        line["app"] = json!(alert.app);
        log.write(line);
    }));
    started.ok()
}

/// Evaluate alert rules every few seconds, until aborted.
async fn alert_loop(daemon: Arc<Daemon>) {
    let mut tick = tokio::time::interval(ALERT_EVERY);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut round = 0u64;
    loop {
        tick.tick().await;
        round = round.wrapping_add(1);
        let apps: Vec<(String, AppEntry)> = {
            let store = daemon.store.lock().await;
            store
                .apps()
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect()
        };
        // Log sizes need a few stat calls per app: once a minute, off the
        // runtime thread.
        let sizes = if round % 12 == 1 {
            let logs: Vec<(String, vexor_core::LogSpec)> = apps
                .iter()
                .map(|(n, e)| (n.clone(), e.spec.logs.clone()))
                .collect();
            tokio::task::spawn_blocking(move || {
                logs.into_iter()
                    .map(|(n, l)| (n, log_disk_bytes(&l)))
                    .collect::<BTreeMap<_, _>>()
            })
            .await
            .ok()
        } else {
            None
        };
        let changes = daemon.evaluate_alerts(&apps, sizes);
        let channels = daemon
            .alerts
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .config
            .channels
            .clone();
        for change in changes {
            if change.notify
                && let Some(n) = &daemon.notifier
            {
                n.notify(&channels, &daemon.alert_of(&change));
            }
            let data = json!({
                "rule": change.key.metric.name(),
                "instance": change.key.instance,
                "level": change.kind,
                "value": change.value,
                "threshold": change.threshold,
                "notify": change.notify,
            });
            daemon
                .events
                .record_daemon(Some(&change.key.app), "alert", data.clone());
            let mut line = data;
            line["type"] = json!("alert");
            line["app"] = json!(change.key.app);
            daemon.log.write(line);
        }
    }
}

/// Serve the Prometheus endpoint configured in settings.json, re-reading
/// it every few seconds; a listener that could not bind is retried.
async fn prometheus_loop(daemon: Arc<Daemon>) {
    let path = daemon.home.join(prometheus::SETTINGS);
    let mut loaded: Option<Option<std::time::SystemTime>> = None;
    let mut server: Option<(std::net::SocketAddr, tokio::task::JoinHandle<()>)> = None;
    let mut tick = tokio::time::interval(ALERT_EVERY);
    loop {
        tick.tick().await;
        let modified = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
        if loaded == Some(modified) {
            continue;
        }
        loaded = Some(modified);
        let mut warnings = Vec::new();
        let parsed = match modified {
            None => Ok(prometheus::Settings::default()),
            Some(_) => std::fs::read_to_string(&path)
                .map_err(|e| e.to_string())
                .and_then(|t| prometheus::Settings::parse(&t)),
        };
        let want = match parsed {
            Ok(s) => s.prometheus.and_then(|p| p.addr().ok()),
            Err(e) => {
                warnings.push(format!(
                    "{} is invalid, keeping the previous settings: {e}",
                    path.display()
                ));
                server.as_ref().map(|s| s.0)
            }
        };
        if server.as_ref().map(|s| s.0) != want {
            if let Some((_, task)) = server.take() {
                task.abort();
            }
            if let Some(addr) = want {
                match tokio::net::TcpListener::bind(addr).await {
                    Ok(l) => {
                        daemon.log.write(
                            json!({ "type": "prometheus_listening", "addr": addr.to_string() }),
                        );
                        server = Some((addr, tokio::spawn(serve_metrics(l, daemon.clone()))));
                    }
                    Err(e) => {
                        warnings.push(format!("Prometheus endpoint: cannot listen on {addr}: {e}"));
                        loaded = None;
                    }
                }
            }
        }
        if let Some((addr, _)) = &server
            && !prometheus::is_loopback(addr)
        {
            warnings.push(format!(
                "the Prometheus endpoint listens on {addr}: other machines can read it, without authentication"
            ));
        }
        *daemon
            .settings_warnings
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = warnings;
    }
}

async fn serve_metrics(listener: tokio::net::TcpListener, daemon: Arc<Daemon>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    loop {
        let Ok((mut stream, _)) = listener.accept().await else {
            tokio::time::sleep(Duration::from_millis(100)).await;
            continue;
        };
        let daemon = daemon.clone();
        tokio::spawn(async move {
            let mut buf = vec![0u8; 8192];
            let mut len = 0;
            // The request head, within 5 s and 8 KiB.
            let read = tokio::time::timeout(REQUEST_TIMEOUT, async {
                while len < buf.len() {
                    let n = stream.read(&mut buf[len..]).await.ok()?;
                    if n == 0 {
                        break;
                    }
                    len += n;
                    if buf[..len].windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
                Some(())
            })
            .await;
            if !matches!(read, Ok(Some(()))) {
                return;
            }
            let head = String::from_utf8_lossy(&buf[..len]);
            let target = head.lines().next().unwrap_or("");
            let (status, body) = if target.starts_with("GET /metrics ") || target == "GET /metrics"
            {
                ("200 OK", daemon.prometheus_text().await)
            } else {
                ("404 Not Found", "Not found: try /metrics\n".to_owned())
            };
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Type: text/plain; version=0.0.4; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes()).await;
            let _ = stream.shutdown().await;
        });
    }
}

/// Bytes of an app's log files on disk, rotated ones included.
fn log_disk_bytes(logs: &vexor_core::LogSpec) -> u64 {
    let mut total = 0;
    for base in [&logs.out, &logs.err] {
        let size = |p: &Path| std::fs::metadata(p).map_or(0, |m| m.len());
        total += size(base);
        for i in 1..=logs.retain {
            total += size(Path::new(&format!("{}.{i}", base.display())));
        }
    }
    total
}

fn observe(
    out: &mut Vec<alerts::Change>,
    engine: &mut alerts::Engine,
    config: &alerts::Config,
    key: alerts::Key,
    value: f64,
    now: u64,
) {
    out.extend(engine.observe(config, key, value, now));
}

struct Handle {
    desired: watch::Sender<Desired>,
    commands: mpsc::Sender<ToShim>,
    live: Live,
    task: tokio::task::JoinHandle<()>,
}

#[derive(Debug, Clone, Copy)]
enum Op {
    StartExisting,
    Stop,
    Restart,
    Reload,
    Delete,
}

impl Daemon {
    /// Write the app's spec file for its shim and start following it.
    fn follow(&self, name: &str, entry: &AppEntry) -> Result<(), String> {
        let spec_path = self.spec_path(name);
        let json = serde_json::to_vec_pretty(&entry.spec).map_err(|e| e.to_string())?;
        vexor_shim::write_atomic(&spec_path, &json).map_err(|e| e.to_string())?;

        let (desired_tx, desired_rx) = watch::channel(entry.desired);
        let (cmd_tx, cmd_rx) = mpsc::channel(8);
        let live = Live::default();
        let follower = Follower {
            home: self.home.clone(),
            name: name.to_owned(),
            spec: spec_path,
            paths: Paths::new(&self.home, name),
            log: self.log.clone(),
            events: self.events.clone(),
            resume: self.events.resume(name),
            live: live.clone(),
            desired: desired_rx,
            commands: cmd_rx,
        };
        let task = tokio::spawn(follower.run());
        let old = self.handles().insert(
            name.to_owned(),
            Handle {
                desired: desired_tx,
                commands: cmd_tx,
                live,
                task,
            },
        );
        if let Some(old) = old {
            old.task.abort();
        }
        Ok(())
    }

    /// One round of alert evaluation over the apps that should be running.
    fn evaluate_alerts(
        &self,
        apps: &[(String, AppEntry)],
        sizes: Option<BTreeMap<String, u64>>,
    ) -> Vec<alerts::Change> {
        use alerts::{Key, Metric};
        let now = describe_now_ms();
        let mut st = self.alerts.lock().unwrap_or_else(PoisonError::into_inner);
        st.reload(&self.home, &self.log);
        if let Some(s) = sizes {
            st.log_disk = s;
        }
        let AlertState {
            config,
            engine,
            restarts,
            log_disk,
            ..
        } = &mut *st;
        let mut out = Vec::new();
        for (name, entry) in apps {
            if entry.desired == Desired::Stopped {
                continue;
            }
            let Some(live) = self.live(name).filter(|l| l.shim_pid().is_some()) else {
                continue;
            };
            let samples = live.metrics();
            // A sample older than three intervals is from a process that
            // stopped being measured.
            let stale = entry.spec.policy.metrics_interval_ms.max(5_000) * 3;
            for s in live.snapshot() {
                let Some(i) = s["instance"].as_u64().and_then(|i| u32::try_from(i).ok()) else {
                    continue;
                };
                let state = s["state"].as_str().unwrap_or("");
                let key = |metric| Key {
                    metric,
                    app: name.clone(),
                    instance: Some(i),
                };
                let flag = |b: bool| if b { 1.0 } else { 0.0 };
                observe(
                    &mut out,
                    engine,
                    config,
                    key(Metric::CrashLoop),
                    flag(state == "errored"),
                    now,
                );
                let rate = restarts.observe(name, i, s["restarts"].as_u64().unwrap_or(0), now);
                #[allow(clippy::cast_precision_loss)]
                observe(
                    &mut out,
                    engine,
                    config,
                    key(Metric::Restarts),
                    rate as f64,
                    now,
                );
                let sample = samples
                    .iter()
                    .find(|m| m.instance == i && s["pid"].as_u64() == Some(u64::from(m.pid)))
                    .filter(|m| now.saturating_sub(m.ts_ms) <= stale);
                let failing = sample
                    .and_then(|m| m.health.as_ref())
                    .is_some_and(|h| !h.ok);
                observe(
                    &mut out,
                    engine,
                    config,
                    key(Metric::Unhealthy),
                    flag(state == "unhealthy" || failing),
                    now,
                );
                if let Some(m) = sample {
                    if let Some(c) = m.cpu_pct {
                        observe(&mut out, engine, config, key(Metric::Cpu), c, now);
                    }
                    #[allow(clippy::cast_precision_loss)]
                    observe(
                        &mut out,
                        engine,
                        config,
                        key(Metric::Memory),
                        m.rss_bytes as f64,
                        now,
                    );
                    if let Some(f) = m.open_fds {
                        #[allow(clippy::cast_precision_loss)]
                        observe(
                            &mut out,
                            engine,
                            config,
                            key(Metric::OpenFiles),
                            f as f64,
                            now,
                        );
                    }
                }
            }
            if let Some(&bytes) = log_disk.get(name) {
                let key = Key {
                    metric: Metric::LogDisk,
                    app: name.clone(),
                    instance: None,
                };
                #[allow(clippy::cast_precision_loss)]
                observe(&mut out, engine, config, key, bytes as f64, now);
            }
        }
        // Stopped, deleted or scaled-down instances: incidents end quietly.
        engine.forget_older_than(now.saturating_sub(60_000));
        restarts.forget_older_than(now.saturating_sub(60_000));
        out
    }

    async fn prometheus_text(&self) -> String {
        let apps: Vec<(String, Desired)> = {
            let store = self.store.lock().await;
            store
                .apps()
                .iter()
                .map(|(k, v)| (k.clone(), v.desired))
                .collect()
        };
        let views: Vec<prometheus::AppView> = apps
            .into_iter()
            .map(|(name, desired)| {
                let live = self.live(&name).unwrap_or_default();
                prometheus::AppView {
                    running: desired == Desired::Running && live.shim_pid().is_some(),
                    snapshot: live.snapshot(),
                    metrics: live.metrics(),
                    name,
                }
            })
            .collect();
        let open: Vec<(String, String, String)> = {
            let st = self.alerts.lock().unwrap_or_else(PoisonError::into_inner);
            st.engine
                .open()
                .into_iter()
                .map(|o| {
                    let level = if o.level == alerts::Level::Danger {
                        "danger"
                    } else {
                        "warning"
                    };
                    (o.key.app, o.key.metric.name().to_owned(), level.to_owned())
                })
                .collect()
        };
        prometheus::render(&views, &open, describe_now_ms())
    }

    fn alert_of(&self, c: &alerts::Change) -> notify::Alert {
        notify::Alert {
            app: c.key.app.clone(),
            instance: c.key.instance,
            rule: c.key.metric.name().to_owned(),
            level: c.kind.to_owned(),
            value: c.value,
            threshold: c.threshold,
            value_text: alerts::value_text(c.key.metric, c.value),
            ts_ms: describe_now_ms(),
            host: self.host.clone(),
        }
    }

    fn alerts_response(&self, recent: Vec<Value>) -> Response {
        let st = self.alerts.lock().unwrap_or_else(PoisonError::into_inner);
        let mut warnings = self.events.warnings();
        warnings.extend(
            self.settings_warnings
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .iter()
                .cloned(),
        );
        warnings.extend(st.error.clone());
        warnings.extend(st.risks.iter().cloned());
        if let Some(n) = &self.notifier {
            let status = n.status.lock().unwrap_or_else(PoisonError::into_inner);
            for (channel, result) in status.iter() {
                if let Err(e) = result {
                    warnings.push(format!("alert delivery ({channel}) is failing: {e}"));
                }
            }
        } else if !st.config.channels.is_empty() {
            warnings.push("alert delivery could not start; alerts are only recorded".into());
        }
        Response::Alerts {
            open: st
                .engine
                .open()
                .iter()
                .filter_map(|o| serde_json::to_value(o).ok())
                .collect(),
            recent,
            rules: serde_json::to_value(&st.config).unwrap_or(Value::Null),
            warnings,
        }
    }

    fn ok(&self, apps: Vec<AppStatus>) -> Response {
        let mut warnings = self.events.warnings();
        warnings.extend(
            self.settings_warnings
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .iter()
                .cloned(),
        );
        Response::Ok { apps, warnings }
    }

    fn ok_one(&self, status: Option<AppStatus>) -> Response {
        self.ok(status.into_iter().collect())
    }

    fn spec_path(&self, name: &str) -> PathBuf {
        self.home.join("apps").join(format!("{name}.json"))
    }

    fn handles(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, Handle>> {
        self.apps.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn live(&self, name: &str) -> Option<Live> {
        self.handles().get(name).map(|h| h.live.clone())
    }

    fn shim_pid(&self, name: &str) -> Option<u32> {
        self.live(name).and_then(|l| l.shim_pid())
    }

    fn status(&self, store: &Store, name: &str) -> Option<AppStatus> {
        let entry = store.get(name)?;
        let live = self.live(name).unwrap_or_default();
        Some(AppStatus {
            name: name.to_owned(),
            desired: entry.desired,
            shim_pid: live.shim_pid(),
            instances: entry.spec.instances,
            snapshot: live.snapshot(),
            metrics: live.metrics(),
            mode: Some(
                if entry.spec.cluster.is_some() {
                    "cluster"
                } else {
                    "fork"
                }
                .to_owned(),
            ),
        })
    }

    async fn handle(&self, request: Request) -> Response {
        match request {
            Request::List => {
                let store = self.store.lock().await;
                let apps = store
                    .apps()
                    .keys()
                    .filter_map(|n| self.status(&store, n))
                    .collect();
                self.ok(apps)
            }
            Request::Version => Response::Version {
                version: env!("CARGO_PKG_VERSION").to_owned(),
            },
            Request::Save => self.save().await,
            Request::Resurrect => self.resurrect().await,
            Request::Describe { name } => {
                let (status, spec) = {
                    let store = self.store.lock().await;
                    match (self.status(&store, &name), store.get(&name)) {
                        (Some(status), Some(entry)) => (status, entry.spec.clone()),
                        _ => return error(ErrorCode::NotFound, format!("no app named {name:?}")),
                    }
                };
                let events = self.events.clone();
                let app = name.clone();
                let recent =
                    tokio::task::spawn_blocking(move || events.query(Some(&app), None, 10))
                        .await
                        .ok()
                        .and_then(Result::ok)
                        .unwrap_or_default();
                Response::Describe {
                    status,
                    spec: Box::new(spec),
                    events: recent,
                    warnings: self.events.warnings(),
                }
            }
            Request::Events {
                app,
                since_ms,
                limit,
            } => {
                let events = self.events.clone();
                let result = tokio::task::spawn_blocking(move || {
                    events.query(app.as_deref(), since_ms, limit)
                })
                .await;
                match result {
                    Ok(Ok(events)) => Response::Events {
                        events,
                        warnings: self.events.warnings(),
                    },
                    Ok(Err(e)) => error(ErrorCode::Internal, e),
                    Err(e) => error(ErrorCode::Internal, e.to_string()),
                }
            }
            Request::Scale { name, instances } => self.scale(&name, instances).await,
            Request::Shutdown => {
                self.log.write(json!({ "type": "shutdown_requested" }));
                // After this answer is sent; the signal handler shuts down.
                tokio::spawn(async {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    let _ = rustix::process::kill_process(
                        rustix::process::getpid(),
                        rustix::process::Signal::TERM,
                    );
                });
                self.ok(Vec::new())
            }
            Request::AlertsTest => {
                let channels = {
                    let mut st = self.alerts.lock().unwrap_or_else(PoisonError::into_inner);
                    st.reload(&self.home, &self.log);
                    st.config.channels.clone()
                };
                if channels.is_empty() {
                    return error(
                        ErrorCode::NotFound,
                        "no alert channels configured (\"channels\" in alerts.json, see docs/alerts.md)",
                    );
                }
                let alert = notify::Alert {
                    app: "vexor".into(),
                    instance: None,
                    rule: "test".into(),
                    level: "warning".into(),
                    value: 0.0,
                    threshold: None,
                    value_text: "test notification".into(),
                    ts_ms: describe_now_ms(),
                    host: self.host.clone(),
                };
                let results =
                    tokio::task::spawn_blocking(move || notify::deliver_now(&channels, &alert))
                        .await
                        .unwrap_or_default();
                Response::AlertsTested {
                    results: results
                        .into_iter()
                        .map(|(channel, r)| {
                            json!({ "channel": channel, "ok": r.is_ok(), "error": r.err() })
                        })
                        .collect(),
                }
            }
            Request::Alerts => {
                let events = self.events.clone();
                let since = describe_now_ms().saturating_sub(7 * 24 * 3_600_000);
                let recent = tokio::task::spawn_blocking(move || {
                    events.query_kind(None, Some("alert"), Some(since), 50)
                })
                .await
                .ok()
                .and_then(Result::ok)
                .unwrap_or_default();
                self.alerts_response(recent)
            }
            Request::Metrics {
                app,
                since_ms,
                until_ms,
            } => {
                if self.store.lock().await.get(&app).is_none() {
                    return error(ErrorCode::NotFound, format!("no app named {app:?}"));
                }
                let events = self.events.clone();
                let result =
                    tokio::task::spawn_blocking(move || events.metrics(&app, since_ms, until_ms))
                        .await;
                match result {
                    Ok(Ok((resolution_ms, points))) => Response::Metrics {
                        resolution_ms: u64::try_from(resolution_ms).unwrap_or(0),
                        points,
                        warnings: self.events.warnings(),
                    },
                    Ok(Err(e)) => error(ErrorCode::Internal, e),
                    Err(e) => error(ErrorCode::Internal, e.to_string()),
                }
            }
            Request::Start { spec, start } => self.start_new(*spec, start).await,
            Request::StartExisting { target } => self.for_each(target, Op::StartExisting).await,
            Request::Stop { target } => self.for_each(target, Op::Stop).await,
            Request::Restart { target } => self.for_each(target, Op::Restart).await,
            Request::Reload { target } => self.for_each(target, Op::Reload).await,
            Request::Delete { target } => self.for_each(target, Op::Delete).await,
        }
    }

    /// Apply `op` to each target app, holding the store lock throughout.
    async fn for_each(&self, target: Target, op: Op) -> Response {
        let mut store = self.store.lock().await;
        let names: Vec<String> = match &target {
            Target::All => store.apps().keys().cloned().collect(),
            Target::Name(n) => {
                if store.get(n).is_none() {
                    return error(ErrorCode::NotFound, format!("no app named {n:?}"));
                }
                vec![n.clone()]
            }
        };
        let mut failed = Vec::new();
        for name in &names {
            let result = match op {
                Op::StartExisting => self.start_existing(&mut store, name).await,
                Op::Stop => self.stop(&mut store, name).await,
                Op::Restart => self.restart(&mut store, name).await,
                Op::Reload => self.reload(&mut store, name).await,
                Op::Delete => self.delete(&mut store, name).await,
            };
            if let Err(e) = result {
                failed.push(format!("{name}: {e}"));
            }
        }
        let apps = names
            .iter()
            .filter_map(|n| self.status(&store, n))
            .collect();
        match failed.len() {
            0 => self.ok(apps),
            n if n == names.len() => error(ErrorCode::Internal, failed.join("; ")),
            _ => error(ErrorCode::Partial, failed.join("; ")),
        }
    }

    async fn start_new(&self, spec: AppSpec, start: bool) -> Response {
        if let Err(e) = vexor_shim::validate(&spec) {
            return error(ErrorCode::InvalidSpec, e);
        }
        let mut store = self.store.lock().await;
        let name = spec.name.clone();
        if let Some(existing) = store.get(&name) {
            // Only the instance count differs: scale to it (only the
            // difference starts or stops), as PM2 does for a re-started file.
            let mut same = spec.clone();
            same.instances = existing.spec.instances;
            if existing.spec != same {
                return error(
                    ErrorCode::Conflict,
                    format!(
                        "an app named {name:?} already exists with a different configuration; \
                         `vexor delete {name}` first, or `vexor restart {name}` to restart it as is"
                    ),
                );
            }
            if existing.spec.instances != spec.instances {
                drop(store);
                if let resp @ Response::Error { .. } = self.scale(&name, spec.instances).await {
                    return resp;
                }
                store = self.store.lock().await;
            }
            if !start {
                return self.ok_one(self.status(&store, &name));
            }
            return match self.start_existing(&mut store, &name).await {
                Ok(()) => self.ok_one(self.status(&store, &name)),
                Err(e) => error(ErrorCode::Internal, e),
            };
        }
        let entry = AppEntry {
            spec,
            desired: if start {
                Desired::Running
            } else {
                Desired::Stopped
            },
        };
        if let Err(e) = store.update(|apps| {
            apps.insert(name.clone(), entry.clone());
        }) {
            return error(ErrorCode::Internal, format!("cannot save state: {e}"));
        }
        if let Err(e) = self.follow(&name, &entry) {
            return error(ErrorCode::Internal, e);
        }
        self.log.write(json!({ "type": "app_added", "app": name }));
        self.events
            .record_daemon(Some(&name), "app_added", json!({}));
        if !start {
            return self.ok_one(self.status(&store, &name));
        }
        // Wait without holding the store, so other apps can start meanwhile
        // (an ecosystem file's apps start in parallel).
        drop(store);
        if let Err(e) = self.wait_connected(&name).await {
            return error(ErrorCode::Internal, e);
        }
        self.wait_settled(&name, entry.spec.instances, None, START_WAIT)
            .await;
        let store = self.store.lock().await;
        self.ok_one(self.status(&store, &name))
    }

    async fn start_existing(&self, store: &mut Store, name: &str) -> Result<(), String> {
        let instances = store.get(name).map_or(1, |e| e.spec.instances);
        self.set_desired(store, name, Desired::Running)?;
        self.wait_connected(name).await?;
        // Revive instances that ended stopped or errored.
        let needs_restart = self
            .live(name)
            .unwrap_or_default()
            .snapshot()
            .iter()
            .any(|s| !matches!(s["state"].as_str(), Some("online" | "starting")));
        if needs_restart {
            let before = self.restart_counts(name);
            self.send(name, ToShim::Restart).await?;
            self.wait_settled(name, instances, Some(before), START_WAIT)
                .await;
        } else {
            self.wait_settled(name, instances, None, START_WAIT).await;
        }
        Ok(())
    }

    async fn stop(&self, store: &mut Store, name: &str) -> Result<(), String> {
        let kill_timeout = store
            .get(name)
            .map(|e| e.spec.policy.to_policy().kill_timeout)
            .unwrap_or_default();
        self.set_desired(store, name, Desired::Stopped)?;
        self.wait_shim_gone(name, kill_timeout + STOP_SLACK).await
    }

    async fn restart(&self, store: &mut Store, name: &str) -> Result<(), String> {
        let was_running = store.get(name).map(|e| e.desired) == Some(Desired::Running)
            && self.shim_pid(name).is_some();
        let kill_timeout = store
            .get(name)
            .map(|e| e.spec.policy.to_policy().kill_timeout)
            .unwrap_or_default();
        let instances = store.get(name).map_or(1, |e| e.spec.instances);
        self.set_desired(store, name, Desired::Running)?;
        self.wait_connected(name).await?;
        if was_running {
            let before = self.restart_counts(name);
            self.send(name, ToShim::Restart).await?;
            self.wait_settled(name, instances, Some(before), kill_timeout + START_WAIT)
                .await;
        } else {
            self.wait_settled(name, instances, None, START_WAIT).await;
        }
        Ok(())
    }

    fn dump_path(&self) -> PathBuf {
        self.home.join(DUMP)
    }

    async fn save(&self) -> Response {
        let store = self.store.lock().await;
        let dump = state::Dump::of(store.apps());
        if let Err(e) = dump.write(&self.dump_path()) {
            return error(ErrorCode::Internal, format!("cannot write {DUMP}: {e}"));
        }
        self.log
            .write(json!({ "type": "saved", "apps": store.apps().len() }));
        let apps = store
            .apps()
            .keys()
            .filter_map(|n| self.status(&store, n))
            .collect();
        self.ok(apps)
    }

    async fn resurrect(&self) -> Response {
        let dump = match state::Dump::read(&self.dump_path()) {
            Ok(Some(d)) => d,
            Ok(None) => {
                return error(
                    ErrorCode::NotFound,
                    format!("nothing saved yet ({DUMP} not found); run `vexor save` first"),
                );
            }
            Err(e) => return error(ErrorCode::Internal, e),
        };
        let mut store = self.store.lock().await;
        let mut problems = Vec::new();
        let mut names = Vec::new();
        for (name, saved) in dump.apps {
            names.push(name.clone());
            match store.get(&name).cloned() {
                Some(current) if current.spec != saved.spec => problems.push(format!(
                    "{name}: an app with this name exists with a different configuration; left as is"
                )),
                Some(_) => {
                    if saved.desired == Desired::Running
                        && let Err(e) = self.start_existing(&mut store, &name).await
                    {
                        problems.push(format!("{name}: {e}"));
                    }
                }
                None => {
                    if let Err(e) = store.update(|apps| {
                        apps.insert(name.clone(), saved.clone());
                    }) {
                        problems.push(format!("{name}: cannot save state: {e}"));
                        continue;
                    }
                    if let Err(e) = self.follow(&name, &saved) {
                        problems.push(format!("{name}: {e}"));
                        continue;
                    }
                    if saved.desired == Desired::Running {
                        if let Err(e) = self.wait_connected(&name).await {
                            problems.push(format!("{name}: {e}"));
                        } else {
                            self.wait_settled(&name, saved.spec.instances, None, START_WAIT)
                                .await;
                        }
                    }
                }
            }
        }
        let apps = names
            .iter()
            .filter_map(|n| self.status(&store, n))
            .collect();
        if problems.is_empty() {
            self.ok(apps)
        } else if problems.len() == names.len() {
            error(ErrorCode::Internal, problems.join("; "))
        } else {
            error(ErrorCode::Partial, problems.join("; "))
        }
    }

    /// Replace each instance's process; start-first where the app's policy
    /// allows (cluster mode, or `reload_strategy: start_first`), so the app
    /// keeps serving. Fails, naming the instances, if a replacement never
    /// became healthy: those keep their previous process.
    async fn reload(&self, store: &mut Store, name: &str) -> Result<(), String> {
        let Some(entry) = store.get(name).cloned() else {
            return Err("not found".into());
        };
        let running = entry.desired == Desired::Running && self.shim_pid(name).is_some();
        if !running {
            // Nothing to replace: start it.
            return self.start_existing(store, name).await;
        }
        let policy = entry.spec.instance_policy();
        let ready = match policy.reload {
            Some(
                vexor_core::ReplacementReady::Signal(t) | vexor_core::ReplacementReady::Stable(t),
            ) => t,
            None => START_WAIT,
        };
        let timeout = policy.kill_timeout + ready + START_WAIT;
        let live = self.live(name).unwrap_or_default();
        if policy.reload.is_none() {
            // Stop-first: a restart.
            let before = self.restart_counts(name);
            self.send(name, ToShim::Reload).await?;
            self.wait_settled(name, entry.spec.instances, Some(before), timeout)
                .await;
            return Ok(());
        }
        // Start-first: each instance reports `reload` succeeded or failed.
        let before = live.reloads();
        self.send(name, ToShim::Reload).await?;
        let deadline = tokio::time::Instant::now() + timeout;
        let instances: Vec<u64> = (0..u64::from(entry.spec.instances)).collect();
        let (mut failed, mut pending);
        loop {
            let now = live.reloads();
            failed = Vec::new();
            pending = Vec::new();
            for i in &instances {
                let (ok0, bad0) = before.get(i).copied().unwrap_or_default();
                let (ok1, bad1) = now.get(i).copied().unwrap_or_default();
                if bad1 > bad0 {
                    failed.push(i.to_string());
                } else if ok1 <= ok0 {
                    pending.push(i.to_string());
                }
            }
            if pending.is_empty() || tokio::time::Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(POLL).await;
        }
        let mut problems = Vec::new();
        if !failed.is_empty() {
            problems.push(format!(
                "the replacement for instance(s) {} did not become healthy; their previous processes are still running",
                failed.join(", ")
            ));
        }
        if !pending.is_empty() {
            problems.push(format!(
                "instance(s) {} did not finish reloading in time",
                pending.join(", ")
            ));
        }
        if problems.is_empty() {
            self.wait_settled(name, entry.spec.instances, None, START_WAIT)
                .await;
            Ok(())
        } else {
            Err(format!(
                "reload did not complete: {} (see `vexor events {name}`)",
                problems.join("; ")
            ))
        }
    }

    async fn delete(&self, store: &mut Store, name: &str) -> Result<(), String> {
        self.stop(store, name).await?;
        store
            .update(|apps| {
                apps.remove(name);
            })
            .map_err(|e| format!("cannot save state: {e}"))?;
        if let Some(h) = self.handles().remove(name) {
            h.task.abort();
        }
        let _ = std::fs::remove_file(self.spec_path(name));
        self.log
            .write(json!({ "type": "app_deleted", "app": name }));
        self.events
            .record_daemon(Some(name), "app_deleted", json!({}));
        Ok(())
    }

    /// Persist the new desired state first, then tell the follower.
    fn set_desired(&self, store: &mut Store, name: &str, desired: Desired) -> Result<(), String> {
        if store.get(name).map(|e| e.desired) != Some(desired) {
            store
                .update(|apps| {
                    if let Some(e) = apps.get_mut(name) {
                        e.desired = desired;
                    }
                })
                .map_err(|e| format!("cannot save state: {e}"))?;
        }
        if let Some(h) = self.handles().get(name) {
            h.desired.send_replace(desired);
        }
        Ok(())
    }

    /// PM2 `scale`: change the instance count, starting or stopping only
    /// the difference. Persisted, so a relaunched shim runs the new count.
    async fn scale(&self, name: &str, instances: u32) -> Response {
        if !(1..=4096).contains(&instances) {
            return error(
                ErrorCode::InvalidSpec,
                "instances must be between 1 and 4096",
            );
        }
        let mut store = self.store.lock().await;
        let Some(entry) = store.get(name).cloned() else {
            return error(ErrorCode::NotFound, format!("no app named {name:?}"));
        };
        let live = self.live(name).unwrap_or_default();
        let running = entry.desired == Desired::Running && live.shim_pid().is_some();
        if running && live.protocol() < 3 {
            return error(
                ErrorCode::Conflict,
                format!(
                    "{name}'s supervisor was started by an older Vexor and can't scale: \
                     run `vexor restart {name}` once, then scale"
                ),
            );
        }
        let mut spec = entry.spec.clone();
        spec.instances = instances;
        let json = match serde_json::to_vec_pretty(&spec) {
            Ok(j) => j,
            Err(e) => return error(ErrorCode::Internal, e.to_string()),
        };
        if let Err(e) = vexor_shim::write_atomic(&self.spec_path(name), &json) {
            return error(ErrorCode::Internal, format!("cannot write spec: {e}"));
        }
        if let Err(e) = store.update(|apps| {
            if let Some(a) = apps.get_mut(name) {
                a.spec.instances = instances;
            }
        }) {
            return error(ErrorCode::Internal, format!("cannot save state: {e}"));
        }
        self.log
            .write(json!({ "type": "scale", "app": name, "instances": instances }));
        if running && let Err(e) = self.send(name, ToShim::Scale { instances }).await {
            return error(ErrorCode::Internal, e);
        }
        drop(store);
        if running {
            // Until exactly the new set of instances has settled.
            let deadline = tokio::time::Instant::now() + START_WAIT;
            while tokio::time::Instant::now() < deadline {
                let snap = live.snapshot();
                let settled = snap.len() == usize::try_from(instances).unwrap_or(usize::MAX)
                    && snap
                        .iter()
                        .all(|s| !matches!(s["state"].as_str(), Some("starting" | "stopping")));
                if settled {
                    break;
                }
                tokio::time::sleep(POLL).await;
            }
        }
        let store = self.store.lock().await;
        self.ok_one(self.status(&store, name))
    }

    async fn send(&self, name: &str, msg: ToShim) -> Result<(), String> {
        let tx = self
            .handles()
            .get(name)
            .map(|h| h.commands.clone())
            .ok_or("app is not being followed")?;
        tx.send(msg)
            .await
            .map_err(|_| "lost contact with the app's shim".to_owned())
    }

    fn restart_counts(&self, name: &str) -> BTreeMap<u64, u64> {
        self.live(name)
            .unwrap_or_default()
            .snapshot()
            .iter()
            .filter_map(|s| Some((s["instance"].as_u64()?, s["restarts"].as_u64()?)))
            .collect()
    }

    /// Wait, at most `timeout`, until every instance has reported a state
    /// other than starting/stopping, and, with `restarted_from`, has a higher
    /// restart count than before. Returning early is fine: the caller shows
    /// whatever state the instances are in.
    async fn wait_settled(
        &self,
        name: &str,
        instances: u32,
        restarted_from: Option<BTreeMap<u64, u64>>,
        timeout: Duration,
    ) {
        let instances = usize::try_from(instances).unwrap_or(usize::MAX);
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let snapshot = self.live(name).unwrap_or_default().snapshot();
            let complete = snapshot.len() >= instances;
            let settled = complete
                && !snapshot.is_empty()
                && snapshot.iter().all(|s| {
                    let state_ok = !matches!(s["state"].as_str(), Some("starting" | "stopping"));
                    let restarted = restarted_from.as_ref().is_none_or(|before| {
                        let i = s["instance"].as_u64().unwrap_or(0);
                        let was = before.get(&i).copied().unwrap_or(0);
                        s["restarts"].as_u64().unwrap_or(0) > was
                    });
                    state_ok && restarted
                });
            if settled || tokio::time::Instant::now() >= deadline {
                return;
            }
            tokio::time::sleep(POLL).await;
        }
    }

    async fn wait_connected(&self, name: &str) -> Result<(), String> {
        let deadline = tokio::time::Instant::now() + SHIM_WAIT;
        loop {
            if self.shim_pid(name).is_some() {
                return Ok(());
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(format!(
                    "its shim did not come up within {SHIM_WAIT:?}; see {}",
                    vexor_shim::shims_dir(&self.home)
                        .join(format!("{name}.log"))
                        .display()
                ));
            }
            tokio::time::sleep(POLL).await;
        }
    }

    async fn wait_shim_gone(&self, name: &str, timeout: Duration) -> Result<(), String> {
        let lock = Paths::new(&self.home, name).lock;
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if self.shim_pid(name).is_none() && matches!(vexor_shim::try_lock(&lock), Ok(Some(_))) {
                return Ok(());
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(format!("still stopping after {timeout:?}"));
            }
            tokio::time::sleep(POLL).await;
        }
    }
}

fn describe_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

fn error(code: ErrorCode, message: impl Into<String>) -> Response {
    Response::Error {
        code,
        message: message.into(),
    }
}

async fn serve(listener: UnixListener, daemon: Arc<Daemon>) {
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                tokio::spawn(client(stream, daemon.clone()));
            }
            Err(e) => {
                daemon
                    .log
                    .write(json!({ "type": "error", "error": format!("accept: {e}") }));
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
}

/// One request, one response.
async fn client(stream: UnixStream, daemon: Arc<Daemon>) {
    if !follower::same_user(&stream) {
        daemon
            .log
            .write(json!({ "type": "client_rejected", "reason": "different user" }));
        return;
    }
    let (read, mut write) = stream.into_split();
    let mut reader = BufReader::new(read);
    let mut buf = Vec::new();
    let req = tokio::time::timeout(
        REQUEST_TIMEOUT,
        vexor_proto::recv::<_, ClientRequest>(&mut reader, &mut buf),
    )
    .await;
    let response = match req {
        Ok(Ok(Some(ClientRequest { protocol, request }))) if protocol == PROTOCOL => {
            let summary = request_summary(&request);
            let response = daemon.handle(request).await;
            if let Response::Error { code, message } = &response {
                daemon.log.write(json!({
                    "type": "request_failed",
                    "request": summary,
                    "code": code,
                    "error": message,
                }));
            }
            response
        }
        Ok(Ok(Some(ClientRequest { protocol, .. }))) => error(
            ErrorCode::Protocol,
            format!(
                "the CLI speaks protocol {protocol}, the daemon {PROTOCOL}: restart the daemon with this version"
            ),
        ),
        Ok(Ok(None)) => return,
        Ok(Err(e)) => error(ErrorCode::Protocol, format!("bad request: {e}")),
        Err(_) => error(ErrorCode::Protocol, "no request received"),
    };
    let _ = vexor_proto::send(&mut write, &response).await;
}

/// The request without its spec (specs carry environment values).
fn request_summary(r: &Request) -> Value {
    match r {
        Request::Start { spec, .. } => json!({ "type": "start", "name": spec.name }),
        other => serde_json::to_value(other).unwrap_or(Value::Null),
    }
}

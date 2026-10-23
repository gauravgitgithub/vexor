//! `vexor shim`: the per-app data plane (docs/architecture.md §2, §7).
//!
//! A shim parents every instance of one app, owns their pipes and log files,
//! and runs the restart loop.
//!
//! Two modes:
//! - Foreground (`vexor shim --spec FILE`): reports events on stdout and exits
//!   once every instance has ended.
//! - Daemon-managed (`--home DIR`): additionally takes the per-app lock, serves
//!   the control socket, records instance identities, and stays up after its
//!   instances end (so their final state stays visible) until told to stop.

mod cluster;
mod control;
mod cron;
mod filewatch;
mod identity;
mod instance;
mod logs;
mod memory;
mod orphan;
mod output;
mod process;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::watch;
use vexor_core::{AppSpec, State};

pub use control::{
    LOCK_WAIT, Paths, bind as bind_socket, lock_with_retry, private_dir, shims_dir, try_lock,
    write_atomic,
};
pub use identity::Identity;

/// Exit codes of `vexor shim`.
pub mod exit {
    /// Every instance ended stopped.
    pub const STOPPED: i32 = 0;
    /// At least one instance ended errored (crash loop).
    pub const ERRORED: i32 = 1;
    /// The spec is unreadable or invalid.
    pub const BAD_SPEC: i32 = 2;
    /// Another shim already supervises this app.
    pub const ALREADY_RUNNING: i32 = 4;
    /// The runtime or signal handlers could not be set up.
    pub const INTERNAL: i32 = 70;
}

/// Load a spec file, then supervise the app. With `home`, run daemon-managed
/// (see the crate docs).
pub fn run_from_file(path: &Path, home: Option<&Path>) -> i32 {
    let spec = match load_spec(path) {
        Ok(s) => s,
        Err(msg) => {
            eprintln!("vexor shim: {msg}");
            return exit::BAD_SPEC;
        }
    };
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("vexor shim: cannot start runtime: {e}");
            return exit::INTERNAL;
        }
    };
    rt.block_on(run(spec, home.map(Path::to_path_buf)))
}

fn load_spec(path: &Path) -> Result<AppSpec, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read spec {}: {e}", path.display()))?;
    let spec: AppSpec =
        serde_json::from_str(&text).map_err(|e| format!("invalid spec {}: {e}", path.display()))?;
    validate(&spec).map_err(|e| format!("invalid spec {}: {e}", path.display()))?;
    Ok(spec)
}

/// Check a spec before it is accepted (also used by the daemon).
pub fn validate(spec: &AppSpec) -> Result<(), String> {
    // The name becomes part of file names under $VEXOR_HOME (security.md T5).
    let name_ok = !spec.name.is_empty()
        && spec.name.len() <= 64
        && !spec.name.starts_with('.')
        && spec
            .name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if !name_ok {
        return Err(format!(
            "`name` must be 1-64 characters of A-Z, a-z, 0-9, '-', '_', '.', not starting with '.', got {:?}",
            spec.name
        ));
    }
    if !spec.exec.is_absolute() {
        return Err(format!(
            "`exec` must be an absolute path, got {}",
            spec.exec.display()
        ));
    }
    if !spec.cwd.is_absolute() {
        return Err(format!(
            "`cwd` must be an absolute path, got {}",
            spec.cwd.display()
        ));
    }
    if !spec.logs.out.is_absolute() || !spec.logs.err.is_absolute() {
        return Err("log paths must be absolute".into());
    }
    if spec.instances == 0 {
        return Err("`instances` must be at least 1".into());
    }
    if let Some(c) = &spec.cluster
        && !c.script.is_absolute()
    {
        return Err(format!(
            "`cluster.script` must be an absolute path, got {}",
            c.script.display()
        ));
    }
    if spec.instance_var.is_empty() || spec.instance_var.contains('=') {
        return Err("`instance_var` must be a non-empty name without '='".into());
    }
    if let Some(c) = &spec.cron_restart {
        cron::parse(c)?;
    }
    if let Some(w) = &spec.watch {
        if w.paths.is_empty() || w.paths.iter().any(|p| !p.is_absolute()) {
            return Err("`watch.paths` must be one or more absolute paths".into());
        }
        filewatch::matcher(w, Vec::new())?;
    }
    Ok(())
}

/// Supervise `spec`. SIGTERM or SIGINT (or `ToShim::Stop`) stops all
/// instances gracefully.
pub async fn run(spec: AppSpec, home: Option<PathBuf>) -> i32 {
    let (stop_tx, stop_rx) = watch::channel(false);
    let stop_tx = Arc::new(stop_tx);
    let (restart_tx, _) = tokio::sync::broadcast::channel::<instance::Control>(16);
    let (mut term, mut int) = match (
        signal(SignalKind::terminate()),
        signal(SignalKind::interrupt()),
    ) {
        (Ok(t), Ok(i)) => (t, i),
        (Err(e), _) | (_, Err(e)) => {
            eprintln!("vexor shim: cannot install signal handlers: {e}");
            return exit::INTERNAL;
        }
    };

    if let Err(e) = orphan::become_subreaper() {
        eprintln!(
            "vexor shim: cannot become child subreaper, escaped descendants won't be tracked: {e}"
        );
    }
    let out = output::Output::new(true);

    // Daemon-managed mode: lock, control socket, persisted identities.
    let mut presence = None;
    let managed = match &home {
        None => orphan::Managed::default(),
        Some(home) => match claim(home, &spec.name) {
            Ok(Claim::Taken) => {
                eprintln!(
                    "vexor shim: another shim already supervises {:?}",
                    spec.name
                );
                return exit::ALREADY_RUNNING;
            }
            Ok(Claim::Ours {
                lock,
                listener,
                paths,
            }) => {
                let me = match Identity::of(std::process::id()) {
                    Ok(me) => me,
                    Err(e) => {
                        eprintln!("vexor shim: cannot read own identity: {e}");
                        return exit::INTERNAL;
                    }
                };
                let server = tokio::spawn(control::serve(
                    listener,
                    out.clone(),
                    me,
                    Arc::from(spec.name.as_str()),
                    stop_tx.clone(),
                    restart_tx.clone(),
                ));
                let managed = orphan::Managed::persisted(paths.pids.clone());
                presence = Some((lock, server, paths));
                managed
            }
            Err(e) => {
                eprintln!("vexor shim: cannot set up {}: {e}", home.display());
                return exit::INTERNAL;
            }
        },
    };
    let logs = match logs::Logs::start(&spec.name, &spec.logs, &out) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("vexor shim: cannot set up log files: {e}");
            return exit::INTERNAL;
        }
    };
    let reaper = tokio::spawn(watch_orphans(
        spec.name.clone(),
        managed.clone(),
        out.clone(),
    ));

    let spec = Arc::new(spec);
    let cluster = spec.cluster.as_ref().map(|_| {
        cluster::Cluster::start(
            spec.clone(),
            managed.clone(),
            logs.out.clone(),
            logs.err.clone(),
        )
    });
    let cron = spec.cron_restart.as_deref().map(|c| {
        let c = cron::parse(c).expect("validated");
        tokio::spawn(cron::schedule(
            c,
            spec.name.clone(),
            out.clone(),
            restart_tx.clone(),
        ))
    });
    let watcher = spec.watch.clone().map(|w| {
        let logs = vec![spec.logs.out.clone(), spec.logs.err.clone()];
        let m = filewatch::matcher(&w, logs).expect("validated");
        tokio::spawn(filewatch::run(
            w,
            m,
            spec.name.clone(),
            out.clone(),
            restart_tx.clone(),
        ))
    });
    let epoch = tokio::time::Instant::now();
    let mut set = tokio::task::JoinSet::new();
    for index in 0..spec.instances {
        let runner = instance::Runner::new(
            spec.clone(),
            index,
            out.clone(),
            epoch,
            managed.clone(),
            (logs.out.clone(), logs.err.clone()),
            cluster.clone(),
        );
        // Standalone, the shim ends once every instance has stopped, unless
        // a schedule or a file change can start them again.
        let exit_when_idle = home.is_none() && spec.cron_restart.is_none() && spec.watch.is_none();
        set.spawn(runner.run(stop_rx.clone(), restart_tx.subscribe(), exit_when_idle));
    }

    let mut states = Vec::with_capacity(set.len());
    loop {
        tokio::select! {
            joined = set.join_next() => match joined {
                Some(Ok(state)) => states.push(state),
                Some(Err(e)) => {
                    eprintln!("vexor shim: instance task failed: {e}");
                    states.push(State::Errored);
                }
                None => break,
            },
            _ = term.recv() => { stop_tx.send_replace(true); }
            _ = int.recv() => { stop_tx.send_replace(true); }
        }
    }

    if let Some(c) = cron {
        c.abort();
    }
    if let Some(w) = watcher {
        w.abort();
    }
    if let Some(c) = &cluster {
        c.shutdown().await;
    }
    drop(cluster);
    reaper.abort();
    logs.finish(std::time::Duration::from_secs(5)).await;
    if spec.treekill {
        orphan::stop_all(&managed, spec.policy.to_policy().kill_timeout).await;
    }
    if let Some((lock, server, paths)) = presence {
        server.abort();
        let _ = std::fs::remove_file(&paths.socket);
        let _ = std::fs::remove_file(&paths.pids);
        drop(lock);
    }
    if states.contains(&State::Errored) {
        exit::ERRORED
    } else {
        exit::STOPPED
    }
}

enum Claim {
    Taken,
    Ours {
        lock: std::fs::File,
        listener: tokio::net::UnixListener,
        paths: control::Paths,
    },
}

fn claim(home: &Path, app: &str) -> std::io::Result<Claim> {
    control::private_dir(home)?;
    control::private_dir(&control::shims_dir(home))?;
    let paths = control::Paths::new(home, app);
    let Some(lock) = control::lock_with_retry(&paths.lock, control::LOCK_WAIT)? else {
        return Ok(Claim::Taken);
    };
    let listener = control::bind(&paths.socket)?;
    Ok(Claim::Ours {
        lock,
        listener,
        paths,
    })
}

/// Reap adopted orphans as they exit, and report each new live one once.
async fn watch_orphans(app: String, managed: orphan::Managed, out: output::Output) {
    let mut sigchld = signal(SignalKind::child()).ok();
    // SIGCHLD can coalesce; a slow periodic sweep catches anything missed.
    let mut sweep = tokio::time::interval(std::time::Duration::from_secs(1));
    let mut seen = std::collections::HashSet::new();
    loop {
        match sigchld.as_mut() {
            Some(sig) => {
                tokio::select! {
                    _ = sig.recv() => {}
                    _ = sweep.tick() => {}
                }
            }
            None => {
                sweep.tick().await;
            }
        }
        let alive = orphan::reap(&managed);
        seen.retain(|id| alive.contains(id));
        for id in alive {
            if seen.insert(id) {
                out.emit(&output::ShimEvent::Orphan {
                    app: &app,
                    pid: id.pid,
                    start: id.start,
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> AppSpec {
        serde_json::from_str(
            r#"{"name":"a","exec":"/bin/true","cwd":"/","logs":{"out":"/tmp/o","err":"/tmp/e"}}"#,
        )
        .expect("valid")
    }

    #[test]
    fn validation() {
        assert!(validate(&spec()).is_ok());
        let cases: [fn(&mut AppSpec); 8] = [
            |s| s.name = "../etc".into(),
            |s| s.name = ".hidden".into(),
            |s| s.name = "x".repeat(65),
            |s| s.exec = "true".into(),
            |s| s.cwd = "rel".into(),
            |s| s.instances = 0,
            |s| s.name.clear(),
            |s| s.instance_var = "A=B".into(),
        ];
        for mutate in cases {
            let mut s = spec();
            mutate(&mut s);
            assert!(validate(&s).is_err(), "{s:?}");
        }
    }
}

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
mod health;
mod identity;
mod instance;
mod logs;
mod memory;
mod metrics;
mod orphan;
mod output;
mod proc;
mod process;
mod routes;
mod upgrade;
mod user;

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
/// (see the crate docs). With `handover`, continue where the previous
/// program image of this shim left off (in-place upgrade).
pub fn run_from_file(path: &Path, home: Option<&Path>, handover: Option<&Path>) -> i32 {
    let spec = match load_spec(path) {
        Ok(s) => s,
        Err(msg) => {
            eprintln!("vexor shim: {msg}");
            return exit::BAD_SPEC;
        }
    };
    let handover = match handover.map(upgrade::read).transpose() {
        Ok(h) => h,
        Err(msg) => {
            // The apps are still our children, but nothing says which pipes
            // are theirs. Exiting frees the lock: the next shim adopts them.
            eprintln!("vexor shim: cannot resume after an upgrade: {msg}");
            return exit::INTERNAL;
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
    rt.block_on(run(spec, home.map(Path::to_path_buf), handover))
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
    if let Some(var) = &spec.increment_var
        && spec.incremented(0).is_none()
    {
        return Err(format!(
            "`increment_var` {var:?} must name a variable of `env` holding a whole number"
        ));
    }
    user::resolve(spec.user.as_deref(), spec.group.as_deref())?;
    if spec.shutdown_with_message && !spec.node_ipc && spec.cluster.is_none() {
        return Err("`shutdown_with_message` needs the Node IPC channel (`node_ipc`)".into());
    }
    if let Some(c) = &spec.cron_restart {
        cron::parse(c)?;
    }
    if let Some(hc) = &spec.health_check {
        hc.target(spec.cluster.is_none().then_some(0))?;
    }
    if let Some(w) = &spec.watch {
        if w.paths.is_empty() || w.paths.iter().any(|p| !p.is_absolute()) {
            return Err("`watch.paths` must be one or more absolute paths".into());
        }
        filewatch::matcher(w, Vec::new())?;
    }
    Ok(())
}

/// How one run of the supervisor ended.
enum Outcome {
    Exit(i32),
    /// `ToShim::Upgrade`: every instance is frozen; replace the program.
    Upgrade {
        exe: PathBuf,
        handover: upgrade::Handover,
    },
}

/// Supervise `spec`. SIGTERM or SIGINT (or `ToShim::Stop`) stops all
/// instances gracefully. `ToShim::Upgrade` replaces the program in place;
/// if that fails, supervision carries on in this program from the same
/// handover.
pub async fn run(spec: AppSpec, home: Option<PathBuf>, handover: Option<upgrade::Handover>) -> i32 {
    let mut handover = handover;
    let mut failed: Option<String> = None;
    loop {
        match run_once(spec.clone(), home.clone(), handover.take(), failed.take()).await {
            Outcome::Exit(code) => return code,
            Outcome::Upgrade { exe, handover: h } => {
                let Some(home) = &home else {
                    return exit::INTERNAL;
                };
                let path = upgrade::path(&control::shims_dir(home), &spec.name);
                let err = match upgrade::write(&path, &h) {
                    Ok(()) => upgrade::exec(&exe, &path),
                    Err(e) => e,
                };
                let _ = std::fs::remove_file(&path);
                eprintln!(
                    "vexor shim: in-place upgrade to {} failed: {err}; carrying on",
                    exe.display()
                );
                failed = Some(format!("exec {}: {err}", exe.display()));
                handover = Some(h);
            }
        }
    }
}

async fn run_once(
    spec: AppSpec,
    home: Option<PathBuf>,
    mut resumed: Option<upgrade::Handover>,
    failed: Option<String>,
) -> Outcome {
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
            return Outcome::Exit(exit::INTERNAL);
        }
    };

    if let Err(e) = orphan::become_subreaper() {
        eprintln!(
            "vexor shim: cannot become child subreaper, escaped descendants won't be tracked: {e}"
        );
    }
    let out = output::Output::new(true);
    if let Some(h) = resumed.as_mut() {
        out.restore(std::mem::take(&mut h.output));
    }
    // `ToShim::Upgrade` from the daemon connection.
    let (upgrade_tx, mut upgrade_rx) = tokio::sync::mpsc::unbounded_channel::<PathBuf>();

    // Latest resource samples, for the daemon connection.
    let (metrics_tx, metrics_rx) = watch::channel(Vec::new());
    // `ToShim::Scale` from the daemon connection: the new instance count.
    let (scale_tx, mut scale_rx) = tokio::sync::mpsc::unbounded_channel::<u32>();

    // Per-route stats are only read by the daemon.
    let route_spec = spec.routes.clone().unwrap_or_default();
    let tap = (home.is_some() && route_spec.enabled)
        .then(|| Arc::new(routes::Tap::new(&route_spec, output::now_ms())));
    let route_interval = std::time::Duration::from_millis(match spec.policy.metrics_interval_ms {
        0 => 5_000,
        ms => ms.max(100),
    });

    // Daemon-managed mode: lock, control socket, persisted identities.
    let mut presence = None;
    let (managed, survivors) = match &home {
        None => (orphan::Managed::default(), Vec::new()),
        Some(home) => match match &resumed {
            Some(h) => reclaim(home, &spec.name, h),
            None => claim(home, &spec.name),
        } {
            Ok(Claim::Taken) => {
                eprintln!(
                    "vexor shim: another shim already supervises {:?}",
                    spec.name
                );
                return Outcome::Exit(exit::ALREADY_RUNNING);
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
                        return Outcome::Exit(exit::INTERNAL);
                    }
                };
                // Kept for an in-place upgrade: the socket stays bound.
                let listener_fd = {
                    use std::os::fd::AsFd;
                    listener.as_fd().try_clone_to_owned().ok()
                };
                let server = tokio::spawn(control::serve(
                    listener,
                    out.clone(),
                    me,
                    Arc::from(spec.name.as_str()),
                    control::Links {
                        stop: stop_tx.clone(),
                        restart: restart_tx.clone(),
                        metrics: metrics_rx.clone(),
                        scale: scale_tx.clone(),
                        routes: tap.clone().map(|t| (t, route_interval)),
                        upgrade: upgrade_tx.clone(),
                    },
                ));
                let (managed, survivors) = orphan::Managed::persisted(paths.pids.clone());
                presence = Some((lock, server, paths, listener_fd));
                // Processes handed over in place are ours, not survivors.
                let ours: Vec<u32> = resumed
                    .iter()
                    .flat_map(|h| &h.instances)
                    .filter_map(|i| i.process.as_ref().map(|p| p.pid))
                    .collect();
                let survivors = survivors
                    .into_iter()
                    .filter(|l| !ours.contains(&l.identity.pid))
                    .collect();
                (managed, survivors)
            }
            Err(e) => {
                eprintln!("vexor shim: cannot set up {}: {e}", home.display());
                return Outcome::Exit(exit::INTERNAL);
            }
        },
    };
    let logs = match logs::Logs::start(&spec.name, &spec.logs, &out, tap) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("vexor shim: cannot set up log files: {e}");
            return Outcome::Exit(exit::INTERNAL);
        }
    };
    // A shim that died left these running: fork-mode instances are taken
    // over as they are; anything else (a cluster primary, which can't be
    // controlled without its channel to the dead shim, or instances beyond
    // the count) is stopped before anything new starts.
    let (adopt, leftovers) = assign_survivors(&survivors, &spec);
    if !leftovers.is_empty() {
        for id in &leftovers {
            out.emit(&output::ShimEvent::LeftoverStopped {
                app: &spec.name,
                pid: id.pid,
            });
        }
        orphan::stop_leftovers(&leftovers, spec.policy.to_policy().kill_timeout).await;
        for id in &leftovers {
            managed.remove(id.pid);
        }
    }
    let mut adopt = adopt;
    // Instances handed over in place, by number.
    let mut inherited: std::collections::BTreeMap<u32, instance::Inherited> = Default::default();
    let mut wanted = spec.instances;
    if let Some(h) = resumed.take() {
        wanted = h.wanted.max(1);
        for i in h.instances {
            let Some(p) = i.process else { continue };
            let fd = |n: Option<i32>| {
                n.and_then(|n| {
                    upgrade::take(n)
                        .map_err(|e| eprintln!("vexor shim: handed-over descriptor {n}: {e}"))
                        .ok()
                })
            };
            inherited.insert(
                i.index,
                instance::Inherited {
                    restarts: i.restarts,
                    identity: Identity {
                        pid: p.pid,
                        start: p.start,
                    },
                    adopted: p.adopted,
                    ipc: fd(p.ipc_fd),
                    stdout: fd(p.stdout_fd),
                    stderr: fd(p.stderr_fd),
                },
            );
        }
        match &failed {
            None => out.emit(&output::ShimEvent::Upgraded {
                app: &spec.name,
                from: &h.from_version,
                to: env!("CARGO_PKG_VERSION"),
            }),
            Some(reason) => out.emit(&output::ShimEvent::UpgradeSkipped {
                app: &spec.name,
                reason,
            }),
        }
    }
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
    let app_health = spec
        .cluster
        .as_ref()
        .and(spec.health_check.as_ref())
        .map(|_| {
            tokio::spawn(cluster_health(
                spec.clone(),
                out.clone(),
                restart_tx.clone(),
                stop_rx.clone(),
            ))
        });
    // Samples are only read by the daemon.
    let sampler = (presence.is_some() && spec.policy.metrics_interval_ms > 0).then(|| {
        tokio::spawn(metrics::run(
            out.clone(),
            std::time::Duration::from_millis(spec.policy.metrics_interval_ms.max(100)),
            metrics_tx,
        ))
    });
    let epoch = tokio::time::Instant::now();
    // Standalone, the shim ends once every instance has stopped, unless a
    // schedule or a file change can start them again.
    let exit_when_idle = home.is_none() && spec.cron_restart.is_none() && spec.watch.is_none();
    let mut set = tokio::task::JoinSet::new();
    // Each instance's own stop: set for all on a global stop, and for the
    // ones a scale-down removes.
    let mut stops: std::collections::BTreeMap<u32, watch::Sender<bool>> = Default::default();
    let spawn = |index: u32,
                 adopt: Option<Identity>,
                 inherit: Option<instance::Inherited>,
                 set: &mut tokio::task::JoinSet<(u32, instance::RunEnd)>,
                 stops: &mut std::collections::BTreeMap<u32, watch::Sender<bool>>| {
        let (tx, rx) = watch::channel(*stop_rx.borrow());
        stops.insert(index, tx);
        let runner = instance::Runner::new(
            spec.clone(),
            index,
            out.clone(),
            epoch,
            managed.clone(),
            (logs.out.clone(), logs.err.clone()),
            cluster.clone(),
        );
        let runner = match (inherit, adopt) {
            (Some(i), _) => runner.inheriting(i),
            (None, Some(id)) => runner.adopting(id),
            (None, None) => runner,
        };
        let control = restart_tx.subscribe();
        set.spawn(async move { (index, runner.run(rx, control, exit_when_idle).await) });
    };
    for index in 0..wanted {
        spawn(
            index,
            adopt.remove(&index),
            inherited.remove(&index),
            &mut set,
            &mut stops,
        );
    }

    let mut global_stop = stop_rx.clone();
    let mut states = Vec::with_capacity(set.len());
    let mut upgrading: Option<PathBuf> = None;
    let mut frozen: Vec<instance::Frozen> = Vec::new();
    loop {
        tokio::select! {
            joined = set.join_next() => match joined {
                Some(Ok((index, instance::RunEnd::Frozen(f)))) => {
                    stops.remove(&index);
                    frozen.push(f);
                }
                Some(Ok((index, instance::RunEnd::Done(state)))) => {
                    stops.remove(&index);
                    let stopping = *stop_rx.borrow();
                    if index >= wanted && !stopping {
                        // Scaled down: the instance is gone, not stopped.
                        out.forget_instance(index);
                        out.emit(&output::ShimEvent::InstanceRemoved { app: &spec.name, instance: index });
                    } else if index < wanted && !stopping && home.is_some() {
                        // Scaled back up while it was stopping.
                        spawn(index, None, None, &mut set, &mut stops);
                    } else {
                        states.push(state);
                    }
                }
                Some(Err(e)) => {
                    eprintln!("vexor shim: instance task failed: {e}");
                    states.push(State::Errored);
                }
                None => break,
            },
            Ok(()) = global_stop.changed() => {
                if *global_stop.borrow() {
                    for tx in stops.values() {
                        tx.send_replace(true);
                    }
                }
            }
            Some(exe) = upgrade_rx.recv() => {
                let reason = if cluster.is_some() {
                    Some("cluster mode: the cluster primary can't be handed over; reload or restart the app to run it under the new version")
                } else if *stop_rx.borrow() {
                    Some("the app is stopping")
                } else if upgrading.is_some() {
                    Some("an upgrade is already in progress")
                } else if !exe.is_absolute() || !exe.is_file() {
                    Some("the new program was not found")
                } else {
                    None
                };
                match reason {
                    Some(reason) => out.emit(&output::ShimEvent::UpgradeSkipped { app: &spec.name, reason }),
                    None => {
                        upgrading = Some(exe);
                        let _ = restart_tx.send(instance::Control::Freeze);
                    }
                }
            }
            Some(n) = scale_rx.recv() => {
                let n = n.max(1);
                if !*stop_rx.borrow() && upgrading.is_none() {
                    for i in wanted.min(n)..wanted.max(n) {
                        if i >= n {
                            if let Some(tx) = stops.get(&i) {
                                tx.send_replace(true);
                            }
                        } else if !stops.contains_key(&i) {
                            spawn(i, None, None, &mut set, &mut stops);
                        }
                    }
                    wanted = n;
                    out.emit(&output::ShimEvent::Scaled { app: &spec.name, instances: n });
                }
            }
            _ = term.recv() => { stop_tx.send_replace(true); }
            _ = int.recv() => { stop_tx.send_replace(true); }
        }
    }

    if let Some(c) = cron.as_ref() {
        c.abort();
    }
    if let Some(w) = watcher {
        w.abort();
    }
    if let Some(h) = app_health {
        h.abort();
    }
    if let Some(s) = sampler {
        s.abort();
    }
    if let (Some(exe), false) = (upgrading, frozen.is_empty()) {
        // Every instance is frozen: collect their pipes once the pumps
        // have forwarded what is buffered, flush the logs, hand over.
        logs.freeze();
        let mut instances = Vec::with_capacity(frozen.len());
        for f in frozen {
            let process = match f.process {
                Some(p) => {
                    let (stdout, stderr) = p.pumps.into_fds().await;
                    Some(upgrade::SavedProcess {
                        pid: p.identity.pid,
                        start: p.identity.start,
                        adopted: p.adopted,
                        ipc_fd: p.ipc.and_then(kept),
                        stdout_fd: stdout.and_then(kept),
                        stderr_fd: stderr.and_then(kept),
                    })
                }
                None => None,
            };
            instances.push(upgrade::SavedInstance {
                index: f.index,
                restarts: f.restarts,
                process,
            });
        }
        logs.finish(std::time::Duration::from_secs(5)).await;
        reaper.abort();
        let (lock, server, _, listener) = presence.expect("only daemon-managed shims upgrade");
        server.abort();
        let handover = upgrade::Handover {
            format: upgrade::FORMAT,
            from_version: env!("CARGO_PKG_VERSION").to_owned(),
            listener_fd: listener.and_then(kept).unwrap_or(-1),
            lock_fd: kept(std::os::fd::OwnedFd::from(lock)).unwrap_or(-1),
            output: out.save(),
            wanted,
            instances,
        };
        return Outcome::Upgrade { exe, handover };
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
    if let Some((lock, server, paths, _)) = presence {
        server.abort();
        let _ = std::fs::remove_file(&paths.socket);
        let _ = std::fs::remove_file(&paths.pids);
        drop(lock);
    }
    Outcome::Exit(if states.contains(&State::Errored) {
        exit::ERRORED
    } else {
        exit::STOPPED
    })
}

/// Keep `fd` open across the exec; `None` (logged) if that failed.
fn kept(fd: std::os::fd::OwnedFd) -> Option<i32> {
    upgrade::keep(fd)
        .map_err(|e| eprintln!("vexor shim: cannot keep a descriptor across the upgrade: {e}"))
        .ok()
}

enum Claim {
    Taken,
    Ours {
        lock: std::fs::File,
        listener: tokio::net::UnixListener,
        paths: control::Paths,
    },
}

/// The lock and control socket the previous program image handed over:
/// already ours, nothing to wait for.
fn reclaim(home: &Path, app: &str, h: &upgrade::Handover) -> std::io::Result<Claim> {
    let lock = std::fs::File::from(upgrade::take(h.lock_fd)?);
    let listener = std::os::unix::net::UnixListener::from(upgrade::take(h.listener_fd)?);
    listener.set_nonblocking(true)?;
    Ok(Claim::Ours {
        lock,
        listener: tokio::net::UnixListener::from_std(listener)?,
        paths: control::Paths::new(home, app),
    })
}

fn claim(home: &Path, app: &str) -> std::io::Result<Claim> {
    control::private_dir(home)?;
    control::private_dir(&control::shims_dir(home))?;
    let paths = control::Paths::new(home, app);
    let Some(lock) = control::lock_with_retry(&paths.lock, control::LOCK_WAIT)? else {
        return Ok(Claim::Taken);
    };
    // Left by an upgrade whose new program died before resuming: its
    // descriptors died with it. The processes are adopted as survivors.
    let _ = std::fs::remove_file(upgrade::path(&control::shims_dir(home), app));
    let listener = control::bind(&paths.socket)?;
    Ok(Claim::Ours {
        lock,
        listener,
        paths,
    })
}

/// Cluster mode: every worker shares the app's port, so a check can't tell
/// workers apart. After `health_restart_after` failures in a row the app is
/// reloaded (new workers first, old ones kept until the new are ready).
/// Failures count only once the app has passed a check, and again only after
/// it passes following a reload, so a check that never passes doesn't cause
/// endless reloads.
async fn cluster_health(
    spec: Arc<AppSpec>,
    out: output::Output,
    control: tokio::sync::broadcast::Sender<instance::Control>,
    mut stopping: watch::Receiver<bool>,
) {
    let Some(hc) = spec.health_check.clone() else {
        return;
    };
    let Ok(target) = hc.target(None) else {
        return;
    };
    let interval = std::time::Duration::from_millis(hc.interval_ms.max(100));
    let timeout = std::time::Duration::from_millis(hc.timeout_ms.max(10));
    let restart_after = spec.policy.health_restart_after.unwrap_or(u32::MAX);
    let mut armed = false;
    let mut failures = 0u32;
    let mut last: Option<bool> = None;
    loop {
        let wait = if armed {
            interval
        } else {
            interval.min(std::time::Duration::from_millis(500))
        };
        // Checks end when the app is being stopped: its failing checks
        // then mean nothing.
        tokio::select! {
            () = tokio::time::sleep(wait) => {}
            _ = stopping.wait_for(|s| *s) => return,
        }
        let result = health::probe(&target, timeout).await;
        if *stopping.borrow() {
            return;
        }
        let ok = result.is_ok();
        if ok {
            armed = true;
            failures = 0;
        } else if armed {
            failures = failures.saturating_add(1);
        }
        if armed && last != Some(ok) {
            last = Some(ok);
            out.emit(&output::ShimEvent::Health {
                app: &spec.name,
                instance: None,
                ok,
                detail: result.as_ref().err().cloned(),
                action: None,
            });
        }
        if armed && failures >= restart_after {
            out.emit(&output::ShimEvent::Health {
                app: &spec.name,
                instance: None,
                ok: false,
                detail: result.err(),
                action: Some("reload"),
            });
            let _ = control.send(instance::Control::Reload);
            armed = false;
            failures = 0;
            last = None;
        }
    }
}

/// Which survivors of a dead shim each instance takes over, and which must
/// be stopped instead: recorded instance numbers first, then (files from
/// before 0.13 have none) the lowest free numbers. Cluster mode adopts
/// nothing.
fn assign_survivors(
    survivors: &[orphan::Leader],
    spec: &AppSpec,
) -> (std::collections::BTreeMap<u32, Identity>, Vec<Identity>) {
    let mut adopt = std::collections::BTreeMap::new();
    let mut leftovers = Vec::new();
    if spec.cluster.is_some() {
        return (adopt, survivors.iter().map(|l| l.identity).collect());
    }
    let mut unnumbered = Vec::new();
    for l in survivors {
        match l.instance {
            Some(i) if i < spec.instances && !adopt.contains_key(&i) => {
                adopt.insert(i, l.identity);
            }
            Some(_) => leftovers.push(l.identity),
            None => unnumbered.push(l.identity),
        }
    }
    for id in unnumbered {
        match (0..spec.instances).find(|i| !adopt.contains_key(i)) {
            Some(i) => {
                adopt.insert(i, id);
            }
            None => leftovers.push(id),
        }
    }
    (adopt, leftovers)
}

/// Reap adopted orphans as they exit, and report each new live one once.
async fn watch_orphans(app: String, managed: orphan::Managed, out: output::Output) {
    let mut sigchld = signal(SignalKind::child()).ok();
    // SIGCHLD can coalesce, and adoption sends none: a periodic sweep
    // catches the rest. Cheap: it reads only this process's children list.
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
    fn survivors_go_to_their_instances_then_to_free_ones() {
        let l = |pid, instance| orphan::Leader {
            identity: Identity { pid, start: 1 },
            instance,
        };
        let mut s = spec();
        s.instances = 3;
        // 0.12 files have no instance numbers; #5 no longer exists; two
        // claim #1 (a reload was in progress).
        let survivors = [
            l(10, Some(1)),
            l(11, None),
            l(12, Some(5)),
            l(13, Some(1)),
            l(14, None),
        ];
        let (adopt, leftovers) = assign_survivors(&survivors, &s);
        let pids: Vec<(u32, u32)> = adopt.iter().map(|(i, id)| (*i, id.pid)).collect();
        assert_eq!(pids, [(0, 11), (1, 10), (2, 14)]);
        let left: Vec<u32> = leftovers.iter().map(|id| id.pid).collect();
        assert_eq!(left, [12, 13]);
        // Cluster mode adopts nothing.
        s.cluster = Some(vexor_core::ClusterSpec {
            script: "/a.js".into(),
            node_args: Vec::new(),
            args: Vec::new(),
        });
        let (adopt, leftovers) = assign_survivors(&survivors, &s);
        assert!(adopt.is_empty());
        assert_eq!(leftovers.len(), 5);
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

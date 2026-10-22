//! `vexor shim`: the per-app data plane (docs/architecture.md §2, §7).
//!
//! A shim parents every instance of one app, owns their pipes and log files,
//! and runs the restart loop. This step (plan 2.3) runs in the foreground from
//! a spec file and reports lifecycle events on stdout; the daemon connection
//! comes in step 2.4b.

mod identity;
mod instance;
mod output;
mod process;

use std::path::Path;
use std::sync::Arc;

use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::watch;
use vexor_core::{AppSpec, State};

pub use identity::Identity;

/// Exit codes of `vexor shim`.
pub mod exit {
    /// Every instance ended stopped.
    pub const STOPPED: i32 = 0;
    /// At least one instance ended errored (crash loop).
    pub const ERRORED: i32 = 1;
    /// The spec is unreadable or invalid.
    pub const BAD_SPEC: i32 = 2;
    /// The runtime or signal handlers could not be set up.
    pub const INTERNAL: i32 = 70;
}

/// Load a spec file, then supervise the app until every instance has ended.
pub fn run_from_file(path: &Path) -> i32 {
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
    rt.block_on(run(spec))
}

fn load_spec(path: &Path) -> Result<AppSpec, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read spec {}: {e}", path.display()))?;
    let spec: AppSpec =
        serde_json::from_str(&text).map_err(|e| format!("invalid spec {}: {e}", path.display()))?;
    validate(&spec).map_err(|e| format!("invalid spec {}: {e}", path.display()))?;
    Ok(spec)
}

fn validate(spec: &AppSpec) -> Result<(), String> {
    if spec.name.is_empty() {
        return Err("`name` is empty".into());
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
    if spec.instance_var.is_empty() || spec.instance_var.contains('=') {
        return Err("`instance_var` must be a non-empty name without '='".into());
    }
    Ok(())
}

/// Supervise `spec` until every instance has ended. SIGTERM or SIGINT stops
/// all instances gracefully.
pub async fn run(spec: AppSpec) -> i32 {
    let (stop_tx, stop_rx) = watch::channel(false);
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

    let spec = Arc::new(spec);
    let epoch = tokio::time::Instant::now();
    let mut set = tokio::task::JoinSet::new();
    for index in 0..spec.instances {
        let runner = instance::Runner::new(spec.clone(), index, output::Output, epoch);
        set.spawn(runner.run(stop_rx.clone()));
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
            _ = term.recv() => { let _ = stop_tx.send(true); }
            _ = int.recv() => { let _ = stop_tx.send(true); }
        }
    }
    if states.contains(&State::Errored) {
        exit::ERRORED
    } else {
        exit::STOPPED
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
        let cases: [fn(&mut AppSpec); 5] = [
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

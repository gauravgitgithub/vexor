//! `vexor doctor`: checks the setup and says what to fix, one line each.
//! Exit status 1 when anything is broken (warnings alone exit 0).

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::ExitCode;

use serde::Serialize;
use vexor_proto::{Desired, Request, Response};

use crate::{client, describe_app, startup, ui};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Level {
    Ok,
    Warning,
    Problem,
}

#[derive(Debug, Serialize)]
pub struct Finding {
    pub level: Level,
    pub check: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fix: Option<String>,
}

fn ok(check: impl Into<String>) -> Finding {
    Finding {
        level: Level::Ok,
        check: check.into(),
        fix: None,
    }
}

fn warn(check: impl Into<String>, fix: impl Into<String>) -> Finding {
    Finding {
        level: Level::Warning,
        check: check.into(),
        fix: Some(fix.into()),
    }
}

fn problem(check: impl Into<String>, fix: impl Into<String>) -> Finding {
    Finding {
        level: Level::Problem,
        check: check.into(),
        fix: Some(fix.into()),
    }
}

/// Bytes available to unprivileged users on the filesystem holding `path`.
fn free_bytes(path: &Path) -> Option<u64> {
    let s = rustix::fs::statvfs(path).ok()?;
    Some(s.f_bavail.saturating_mul(s.f_frsize))
}

/// Network filesystems, where SQLite's WAL mode is unsafe.
#[cfg(target_os = "linux")]
fn network_fs(path: &Path) -> Option<&'static str> {
    let s = rustix::fs::statfs(path).ok()?;
    #[allow(clippy::unnecessary_cast)]
    match s.f_type as i64 {
        0x6969 => Some("NFS"),
        0x517B => Some("SMB"),
        0xFF53_4D42 => Some("CIFS"),
        0xFE53_4D42 => Some("SMB2"),
        _ => None,
    }
}

#[cfg(not(target_os = "linux"))]
fn network_fs(path: &Path) -> Option<&'static str> {
    let s = rustix::fs::statfs(path).ok()?;
    let name: Vec<u8> = s
        .f_fstypename
        .iter()
        .take_while(|c| **c != 0)
        .map(|c| *c as u8)
        .collect();
    match name.as_slice() {
        b"nfs" => Some("NFS"),
        b"smbfs" => Some("SMB"),
        b"afpfs" => Some("AFP"),
        _ => None,
    }
}

pub fn checks(home: &Path) -> Vec<Finding> {
    let mut out = Vec::new();

    // Home directory: private, local.
    match std::fs::metadata(home) {
        Ok(m) if m.permissions().mode() & 0o077 != 0 => out.push(warn(
            format!("{} is readable by other users", home.display()),
            format!("chmod 700 {}", home.display()),
        )),
        Ok(_) => out.push(ok(format!("{} is private to you", home.display()))),
        Err(_) => out.push(ok(format!(
            "{} will be created on first use",
            home.display()
        ))),
    }
    if let Some(fs) = network_fs(home) {
        out.push(problem(
            format!(
                "{} is on a network filesystem ({fs}): the event database is not safe there",
                home.display()
            ),
            "use a local directory with --home or VEXOR_HOME",
        ));
    }

    // The daemon, and everything it reports.
    match client::request(home, &Request::Version) {
        Ok(Response::Version { version }) if version == env!("CARGO_PKG_VERSION") => {
            out.push(ok(format!("daemon {version} is answering")));
        }
        Ok(Response::Version { version }) => out.push(warn(
            format!(
                "the daemon runs {version}, this command is {}",
                env!("CARGO_PKG_VERSION")
            ),
            "any vexor command restarts it with this version; apps keep running",
        )),
        Ok(_) => out.push(warn(
            "the daemon predates version checks",
            "run any vexor command to restart it",
        )),
        Err(e) => {
            out.push(problem(
                format!("the daemon can't be reached: {e}"),
                format!(
                    "see {} and {}",
                    home.join("daemon.err").display(),
                    home.join("daemon.log").display()
                ),
            ));
            return out;
        }
    }
    let apps = match client::request(home, &Request::List) {
        Ok(Response::Ok { apps, warnings }) => {
            for w in warnings {
                out.push(warn(w, "see docs/architecture.md §8 for recovery"));
            }
            apps
        }
        _ => Vec::new(),
    };
    if let Ok(Response::Alerts { warnings, open, .. }) = client::request(home, &Request::Alerts) {
        for w in warnings {
            out.push(warn(w, "see docs/alerts.md and docs/prometheus.md"));
        }
        if !open.is_empty() {
            out.push(warn(
                format!("{} alert incident(s) open", open.len()),
                "vexor alerts",
            ));
        }
    }

    // Apps: supervised, and not stuck.
    let mut healthy = 0;
    for a in &apps {
        if a.desired == Desired::Running && a.shim_pid.is_none() {
            out.push(problem(
                format!(
                    "{}: should run, but its supervisor is not connected",
                    a.name
                ),
                format!(
                    "vexor restart {}; see {}",
                    a.name,
                    home.join("shims").join(format!("{}.log", a.name)).display()
                ),
            ));
            continue;
        }
        let errored = a
            .snapshot
            .iter()
            .filter(|s| s["state"] == "errored")
            .count();
        if errored > 0 {
            out.push(warn(
                format!(
                    "{}: {errored} instance(s) gave up restarting (crash loop)",
                    a.name
                ),
                format!(
                    "vexor logs {} --err; fix it, then vexor restart {}",
                    a.name, a.name
                ),
            ));
            continue;
        }
        let adopted = a.snapshot.iter().filter(|s| s["adopted"] == true).count();
        if adopted > 0 {
            out.push(warn(
                format!(
                    "{}: {adopted} instance(s) kept running after their supervisor died; their output is not captured",
                    a.name
                ),
                format!("vexor restart {} (or reload) when convenient", a.name),
            ));
        }
        healthy += 1;
        if let Ok((_, spec, _)) = describe_app(home, a.name.clone()) {
            if !spec.exec.exists() {
                out.push(problem(
                    format!("{}: {} no longer exists", a.name, spec.exec.display()),
                    "reinstall it, or start the app again with the new path",
                ));
            }
            if let Some(dir) = spec.logs.out.parent()
                && let Some(free) = free_bytes(dir)
            {
                if free < 100 << 20 {
                    out.push(problem(
                        format!(
                            "{}: {} MiB free for its logs in {}",
                            a.name,
                            free >> 20,
                            dir.display()
                        ),
                        "free disk space, or lower max_size/retain",
                    ));
                } else if free < 1 << 30 {
                    out.push(warn(
                        format!(
                            "{}: under 1 GiB free for its logs in {}",
                            a.name,
                            dir.display()
                        ),
                        "free disk space, or lower max_size/retain",
                    ));
                }
            }
        }
    }
    if !apps.is_empty() {
        out.push(ok(format!(
            "{healthy} of {} app(s) supervised and not crash-looping",
            apps.len()
        )));
    }

    // Limits: apps inherit the daemon's.
    let nofile = rustix::process::getrlimit(rustix::process::Resource::Nofile);
    match nofile.current {
        Some(n) if n < 4096 => out.push(warn(
            format!("open-file limit is {n}: busy servers can run out"),
            "raise it (ulimit -n 65535, LimitNOFILE= in the systemd unit) and restart the daemon",
        )),
        _ => out.push(ok("open-file limit is high enough")),
    }

    // Start at boot.
    let user = std::env::var("USER").unwrap_or_default();
    let installed = match startup::Init::detect() {
        Ok(startup::Init::Systemd) => Some(startup::systemd_path(&user).exists()),
        Ok(startup::Init::Launchd) => std::env::var_os("HOME").map(|h| {
            Path::new(&h)
                .join("Library/LaunchAgents")
                .join(format!("{}.plist", startup::launchd_label(&user)))
                .exists()
        }),
        Err(_) => None,
    };
    match installed {
        Some(true) => out.push(ok("the daemon starts at boot")),
        Some(false) => out.push(warn("the daemon does not start at boot", "vexor startup")),
        None => {}
    }
    out
}

pub fn run(home: &Path, json: bool) -> ExitCode {
    let findings = checks(home);
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&findings).unwrap_or_default()
        );
    } else {
        for f in &findings {
            let mark = match f.level {
                Level::Ok => ui::ok_mark(),
                Level::Warning => ui::warn_mark(),
                Level::Problem => ui::fail_mark(),
            };
            println!("{mark} {}", f.check);
            if let Some(fix) = &f.fix {
                println!("    {}", ui::dim(&format!("fix: {fix}")));
            }
        }
    }
    if findings.iter().any(|f| f.level == Level::Problem) {
        ExitCode::from(crate::code::FAILED)
    } else {
        ExitCode::SUCCESS
    }
}

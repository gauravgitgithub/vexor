//! PM2 commands that act on an app's files or processes directly:
//! `flush` (empty its logs) and `signal` (PM2 `sendSignal`).

use std::process::ExitCode;

use vexor_proto::{Request, Response};

use crate::{client, code, describe_app, fail};

/// Names of every app, or just `target` (`all` means every app).
fn names(home: &std::path::Path, target: Option<String>) -> Result<Vec<String>, ExitCode> {
    match target.filter(|t| t != "all") {
        Some(n) => Ok(vec![n]),
        None => match client::request(home, &Request::List) {
            Ok(Response::Ok { apps, .. }) => Ok(apps.into_iter().map(|a| a.name).collect()),
            Ok(Response::Error { message, .. }) => Err(fail(&message)),
            Ok(_) => Err(fail("unexpected answer from the daemon")),
            Err(e) => {
                eprintln!("vexor: {e}");
                Err(ExitCode::from(code::DAEMON_UNREACHABLE))
            }
        },
    }
}

/// `vexor flush [app]`: empty the log files and remove rotated ones. The
/// app keeps writing to the same (now empty) files.
pub fn flush(home: &std::path::Path, target: Option<String>) -> ExitCode {
    let names = match names(home, target) {
        Ok(n) => n,
        Err(c) => return c,
    };
    let mut files = 0;
    for name in names {
        let spec = match describe_app(home, name) {
            Ok((_, spec, _)) => spec,
            Err((c, msg)) => {
                eprintln!("vexor: {msg}");
                return ExitCode::from(c);
            }
        };
        let l = &spec.logs;
        let mut paths = vec![l.out.clone()];
        if l.err != l.out {
            paths.push(l.err.clone());
        }
        for base in paths {
            // Truncate, don't delete: the shim keeps its file open.
            match std::fs::OpenOptions::new().write(true).open(&base) {
                Ok(f) => {
                    if let Err(e) = f.set_len(0) {
                        return fail(&format!("cannot empty {}: {e}", base.display()));
                    }
                    files += 1;
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return fail(&format!("cannot open {}: {e}", base.display())),
            }
            for i in 1..=l.retain {
                let rotated = std::path::PathBuf::from(format!("{}.{i}", base.display()));
                if std::fs::remove_file(&rotated).is_ok() {
                    files += 1;
                }
            }
        }
    }
    println!(
        "{} flushed {files} log file{}",
        crate::ui::ok_mark(),
        if files == 1 { "" } else { "s" }
    );
    ExitCode::SUCCESS
}

/// Signals `vexor signal` accepts, by name with or without `SIG`.
pub fn parse_signal(s: &str) -> Option<rustix::process::Signal> {
    use rustix::process::Signal;
    let s = s.trim().to_ascii_uppercase();
    Some(match s.strip_prefix("SIG").unwrap_or(&s) {
        "HUP" => Signal::HUP,
        "INT" => Signal::INT,
        "QUIT" => Signal::QUIT,
        "KILL" => Signal::KILL,
        "USR1" => Signal::USR1,
        "USR2" => Signal::USR2,
        "TERM" => Signal::TERM,
        "CONT" => Signal::CONT,
        "STOP" => Signal::STOP,
        "WINCH" => Signal::WINCH,
        _ => return None,
    })
}

/// `vexor signal <SIGNAL> <app>`: send a signal to every running instance
/// of the app. Each process is identity-checked first, so a PID reused by
/// another process is never signalled.
pub fn signal(home: &std::path::Path, sig: &str, target: String) -> ExitCode {
    let Some(signal) = parse_signal(sig) else {
        return fail(&format!(
            "unknown signal {sig:?}: use HUP, INT, QUIT, KILL, USR1, USR2, TERM, CONT, STOP or WINCH"
        ));
    };
    let names = match names(home, Some(target)) {
        Ok(n) => n,
        Err(c) => return c,
    };
    let mut sent = 0;
    for name in names {
        let status = match describe_app(home, name.clone()) {
            Ok((status, _, _)) => status,
            Err((c, msg)) => {
                eprintln!("vexor: {msg}");
                return ExitCode::from(c);
            }
        };
        for s in &status.snapshot {
            let (Some(pid), Some(start)) = (s["pid"].as_u64(), s["start"].as_u64()) else {
                continue;
            };
            let Ok(pid32) = u32::try_from(pid) else {
                continue;
            };
            let id = vexor_shim::Identity { pid: pid32, start };
            if !id.is_alive() {
                continue;
            }
            let Some(p) = rustix::process::Pid::from_raw(i32::try_from(pid32).unwrap_or(0)) else {
                continue;
            };
            match rustix::process::kill_process(p, signal) {
                Ok(()) => {
                    sent += 1;
                    println!(
                        "{} {name} #{}: sent {sig} to pid {pid}",
                        crate::ui::ok_mark(),
                        s["instance"]
                    );
                }
                Err(e) => eprintln!("vexor: {name} #{}: pid {pid}: {e}", s["instance"]),
            }
        }
    }
    if sent == 0 {
        return fail("no running instance to signal");
    }
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signal_names() {
        use rustix::process::Signal;
        assert_eq!(parse_signal("SIGUSR2"), Some(Signal::USR2));
        assert_eq!(parse_signal("usr1"), Some(Signal::USR1));
        assert_eq!(parse_signal("9"), None);
        assert_eq!(parse_signal("SIGBOGUS"), None);
    }
}

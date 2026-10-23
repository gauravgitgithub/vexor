//! CLI side of the daemon socket. Starts the daemon in the background when
//! none is running, like PM2 does.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use vexor_proto::{ClientRequest, MAX_LINE, PROTOCOL, Request, Response};

const DAEMON_START_TIMEOUT: Duration = Duration::from_secs(5);
/// Long enough for `stop` of an app with a long kill_timeout.
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(300);

pub fn request(home: &Path, req: &Request) -> Result<Response, String> {
    if *req != Request::Version {
        ensure_current_daemon(home);
    }
    send(home, req)
}

fn send(home: &Path, req: &Request) -> Result<Response, String> {
    let mut stream = connect_or_start(home)?;
    stream
        .set_read_timeout(Some(RESPONSE_TIMEOUT))
        .map_err(|e| e.to_string())?;
    let msg = ClientRequest {
        protocol: PROTOCOL,
        request: req.clone(),
    };
    let mut line = serde_json::to_vec(&msg).map_err(|e| e.to_string())?;
    line.push(b'\n');
    stream
        .write_all(&line)
        .map_err(|e| format!("cannot talk to the daemon: {e}"))?;

    let mut reader = BufReader::new(stream).take(u64::try_from(MAX_LINE).unwrap_or(u64::MAX));
    let mut buf = Vec::new();
    reader
        .read_until(b'\n', &mut buf)
        .map_err(|e| format!("no answer from the daemon: {e}"))?;
    if buf.is_empty() {
        return Err("the daemon closed the connection without answering".into());
    }
    serde_json::from_slice(&buf).map_err(|e| format!("unreadable answer from the daemon: {e}"))
}

/// A daemon left running by another version of Vexor (e.g. before an
/// upgrade) may not understand this CLI's requests. Once per run: if the
/// daemon answers with a different version, or predates the question,
/// stop it gracefully so a current one starts. Apps keep running: they
/// belong to their shims, which the new daemon reconnects to.
fn ensure_current_daemon(home: &Path) {
    static CHECKED: std::sync::Once = std::sync::Once::new();
    CHECKED.call_once(|| {
        let socket = home.join(vexor_daemon::SOCKET);
        if UnixStream::connect(&socket).is_err() {
            return; // None running: the one we start is current.
        }
        let mine = env!("CARGO_PKG_VERSION");
        let theirs = match send(home, &Request::Version) {
            Ok(Response::Version { version }) if version == mine => return,
            Ok(Response::Version { version }) => version,
            Ok(_) => "an older version".to_owned(),
            Err(_) => return,
        };
        let Some(pid) = daemon_pid(home) else {
            eprintln!(
                "vexor: warning: the background daemon is {theirs}, this is {mine}; restart it to avoid errors"
            );
            return;
        };
        if let Some(p) = rustix::process::Pid::from_raw(pid) {
            let _ = rustix::process::kill_process(p, rustix::process::Signal::TERM);
        }
        // Wait until it has let go of its lock.
        let lock = home.join(vexor_daemon::LOCK);
        let deadline = Instant::now() + Duration::from_secs(15);
        while Instant::now() < deadline {
            if matches!(vexor_shim::try_lock(&lock), Ok(Some(_))) {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        eprintln!(
            "vexor: the background daemon was {theirs}; restarted it as {mine} (apps keep running)"
        );
        // Let the new daemon reconnect to the running apps' shims, so the
        // answer to this command shows them as they are.
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            match send(home, &Request::List) {
                Ok(Response::Ok { apps, .. })
                    if apps.iter().all(|a| {
                        a.desired == vexor_proto::Desired::Stopped || !a.snapshot.is_empty()
                    }) =>
                {
                    break;
                }
                Err(_) => break,
                _ => std::thread::sleep(Duration::from_millis(100)),
            }
        }
    });
}

/// The running daemon's PID, from its log.
fn daemon_pid(home: &Path) -> Option<i32> {
    let log = std::fs::read_to_string(home.join("daemon.log")).ok()?;
    log.lines()
        .rev()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .find(|v| v["type"] == "daemon_started")
        .and_then(|v| v["pid"].as_i64())
        .and_then(|p| i32::try_from(p).ok())
        .filter(|p| *p > 1)
}

fn connect_or_start(home: &Path) -> Result<UnixStream, String> {
    let socket = home.join(vexor_daemon::SOCKET);
    if let Ok(s) = UnixStream::connect(&socket) {
        return Ok(s);
    }
    // Lock held: a daemon is running (perhaps still starting). Otherwise
    // start one.
    let lock = home.join(vexor_daemon::LOCK);
    let free = vexor_shim::private_dir(home)
        .and_then(|()| vexor_shim::try_lock(&lock))
        .map_err(|e| format!("cannot use {}: {e}", home.display()))?
        .is_some();
    if free {
        start_daemon(home).map_err(|e| format!("cannot start the daemon: {e}"))?;
    }
    let deadline = Instant::now() + DAEMON_START_TIMEOUT;
    loop {
        match UnixStream::connect(&socket) {
            Ok(s) => return Ok(s),
            Err(e) if Instant::now() >= deadline => {
                return Err(format!(
                    "the daemon is not answering on {} ({e}); see {}",
                    socket.display(),
                    home.join("daemon.err").display()
                ));
            }
            Err(_) => std::thread::sleep(Duration::from_millis(20)),
        }
    }
}

fn start_daemon(home: &Path) -> io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    use std::os::unix::process::CommandExt;

    let err_log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(home.join("daemon.err"))?;
    // Not waited on: the daemon outlives this short-lived CLI process and is
    // reaped by init once we exit.
    #[allow(clippy::zombie_processes)]
    Command::new(std::env::current_exe()?)
        .arg("--home")
        .arg(home)
        .arg("daemon")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(err_log)
        // Detached from this terminal's process group, so Ctrl-C here
        // doesn't reach it.
        .process_group(0)
        .spawn()?;
    Ok(())
}

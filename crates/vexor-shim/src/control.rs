//! The shim's control socket and on-disk presence under `$VEXOR_HOME/shims/`.
//!
//! - `<app>.lock`: held (flock) for the shim's whole life. Exactly one shim per
//!   app; the daemon tests it to tell a live shim from a dead one.
//! - `<app>.sock`: control socket, owner-only. One daemon connection at a
//!   time; a new connection replaces the old one.
//! - `<app>.pids.json`: identities of the instance leaders, so that after a
//!   shim crash the daemon can refuse to start duplicates.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use rustix::fs::{FlockOperation, flock};
use tokio::io::BufReader;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{broadcast, mpsc, watch};
use vexor_proto::{FromShim, InstanceMetrics, MIN_SHIM_PROTOCOL, SHIM_PROTOCOL, ToShim};

use crate::identity::Identity;
use crate::output::Output;

const HELLO_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone)]
pub struct Paths {
    pub lock: PathBuf,
    pub socket: PathBuf,
    pub pids: PathBuf,
}

impl Paths {
    pub fn new(home: &Path, app: &str) -> Paths {
        let dir = shims_dir(home);
        Paths {
            lock: dir.join(format!("{app}.lock")),
            socket: dir.join(format!("{app}.sock")),
            pids: dir.join(format!("{app}.pids.json")),
        }
    }
}

pub fn shims_dir(home: &Path) -> PathBuf {
    home.join("shims")
}

/// Create `dir` (and parents) and restrict it to the owner.
pub fn private_dir(dir: &Path) -> io::Result<()> {
    std::fs::create_dir_all(dir)?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
}

/// Take the exclusive lock at `path`. `Ok(None)` if another process holds it.
/// The returned file must stay open for as long as the lock is needed.
pub fn try_lock(path: &Path) -> io::Result<Option<File>> {
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    match flock(&file, FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => Ok(Some(file)),
        Err(rustix::io::Errno::WOULDBLOCK) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Like [`try_lock`], but keeps trying for `wait`. Others briefly take the
/// lock just to check whether its owner is alive; a process starting up must
/// not mistake such a check for a running owner.
pub fn lock_with_retry(path: &Path, wait: Duration) -> io::Result<Option<File>> {
    let deadline = std::time::Instant::now() + wait;
    loop {
        if let Some(f) = try_lock(path)? {
            return Ok(Some(f));
        }
        if std::time::Instant::now() >= deadline {
            return Ok(None);
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// How long a starting daemon or shim retries its lock.
pub const LOCK_WAIT: Duration = Duration::from_millis(500);

/// Bind the control socket. Any file already at `path` is stale: the caller
/// holds the lock, so no other shim for this app can be using it.
pub fn bind(path: &Path) -> io::Result<UnixListener> {
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let listener = UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

/// Write `contents` to `path` atomically (temp file, fsync, rename).
pub fn write_atomic(path: &Path, contents: &[u8]) -> io::Result<()> {
    use std::io::Write;
    let tmp = path.with_extension("tmp");
    let mut f = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&tmp)?;
    f.write_all(contents)?;
    f.sync_all()?;
    std::fs::rename(&tmp, path)?;
    // Make the rename itself durable.
    if let Some(dir) = path.parent() {
        File::open(dir)?.sync_all()?;
    }
    Ok(())
}

/// What a daemon connection can drive in the shim.
#[derive(Clone)]
pub struct Links {
    pub stop: Arc<watch::Sender<bool>>,
    pub restart: broadcast::Sender<crate::instance::Control>,
    pub metrics: watch::Receiver<Vec<InstanceMetrics>>,
    pub scale: mpsc::UnboundedSender<u32>,
    /// Per-route stats, taken once per interval.
    pub routes: Option<(Arc<crate::routes::Tap>, Duration)>,
    /// `ToShim::Upgrade`: the program to replace this one with.
    pub upgrade: mpsc::UnboundedSender<PathBuf>,
}

/// Accept daemon connections until the task is aborted. The current
/// session ends with it (an upgrade that resumes in this program must not
/// leave a session of the old state behind).
pub async fn serve(listener: UnixListener, out: Output, me: Identity, app: Arc<str>, links: Links) {
    // Held only for its drop: it aborts the session it guards.
    let mut _session: Option<AbortOnDrop> = None;
    loop {
        let stream = match listener.accept().await {
            Ok((s, _)) => s,
            Err(e) => {
                eprintln!("vexor shim: accept failed: {e}");
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        if !same_user(&stream) {
            continue;
        }
        // Replacing it aborts the previous session.
        _session = Some(AbortOnDrop(tokio::spawn(session(
            stream,
            out.clone(),
            me,
            app.clone(),
            links.clone(),
        ))));
    }
}

/// The socket is already owner-only; this also rejects any peer with a
/// different effective UID (docs/security.md T1).
fn same_user(stream: &UnixStream) -> bool {
    match stream.peer_cred() {
        Ok(cred) => cred.uid() == rustix::process::geteuid().as_raw(),
        Err(_) => false,
    }
}

async fn session(stream: UnixStream, out: Output, me: Identity, app: Arc<str>, links: Links) {
    let Links {
        stop,
        restart,
        mut metrics,
        scale,
        routes,
        upgrade,
    } = links;
    let (read, mut write) = stream.into_split();

    // Reading isn't cancel-safe (a partial line would be lost), so it runs
    // in its own task. It hands the hello over, and once the session has
    // accepted it, carries out each command as soon as it is read: a
    // command is never lost because sending events failed, or because a
    // new connection replaced this one. The channel closes with the
    // connection.
    let (tx, mut rx) = mpsc::channel::<ToShim>(1);
    let (accepted, go) = tokio::sync::oneshot::channel::<()>();
    let reader = tokio::spawn(async move {
        let mut r = BufReader::new(read);
        let mut buf = Vec::new();
        let Ok(Some(hello)) = vexor_proto::recv::<_, ToShim>(&mut r, &mut buf).await else {
            return;
        };
        if tx.send(hello).await.is_err() || go.await.is_err() {
            return;
        }
        while let Ok(Some(msg)) = vexor_proto::recv::<_, ToShim>(&mut r, &mut buf).await {
            match msg {
                ToShim::Stop => {
                    stop.send_replace(true);
                }
                ToShim::Restart => {
                    let _ = restart.send(crate::instance::Control::Restart);
                }
                ToShim::Reload => {
                    let _ = restart.send(crate::instance::Control::Reload);
                }
                ToShim::Scale { instances } => {
                    let _ = scale.send(instances);
                }
                ToShim::Upgrade { exec } => {
                    let _ = upgrade.send(exec);
                }
                ToShim::Hello { .. } => {}
            }
        }
        drop(tx);
    });
    // Not aborted with the session: it ends at end of file, after carrying
    // out every command the peer sent before closing.
    drop(reader);

    let supported = MIN_SHIM_PROTOCOL..=SHIM_PROTOCOL;
    let (protocol, resume) = match tokio::time::timeout(HELLO_TIMEOUT, rx.recv()).await {
        Ok(Some(ToShim::Hello { protocol, resume })) if supported.contains(&protocol) => {
            (protocol, resume)
        }
        Ok(Some(ToShim::Hello { protocol, .. })) => {
            let reason = format!(
                "protocol {protocol} not supported, shim speaks {MIN_SHIM_PROTOCOL} to {SHIM_PROTOCOL}"
            );
            let _ = vexor_proto::send(&mut write, &FromShim::Refused { reason }).await;
            return;
        }
        _ => return,
    };
    let _ = accepted.send(());

    let next_seq = out.next_seq();
    let mut cursor = match resume {
        Some(r) if r.shim_start == me.start && r.last_seq < next_seq => r.last_seq + 1,
        // No position, or one from an earlier shim: replay everything
        // still buffered (the daemon ignores events it already stored).
        _ => 0,
    };
    let welcome = FromShim::Welcome {
        protocol,
        app: app.to_string(),
        shim_pid: me.pid,
        shim_start: me.start,
        next_seq,
        snapshot: out.snapshot(),
        // Daemons before protocol 5 refuse fields they don't know.
        version: (protocol >= 5).then(|| env!("CARGO_PKG_VERSION").to_owned()),
    };
    if vexor_proto::send(&mut write, &welcome).await.is_err() {
        return;
    }
    // Daemons before protocol 2 would reject metrics messages.
    let mut send_metrics = protocol >= 2;
    metrics.mark_changed();
    // And before protocol 4, route windows. The first tick sends what was
    // read while no daemon was connected.
    let routes = routes.filter(|_| protocol >= 4);
    let mut route_tick = routes
        .as_ref()
        .map(|(_, every)| tokio::time::interval(*every));

    loop {
        let changed = out.changed();
        tokio::pin!(changed);
        changed.as_mut().enable();

        let batch = out.since(cursor);
        if let Some((from, to)) = batch.gap
            && vexor_proto::send(&mut write, &FromShim::Gap { from, to })
                .await
                .is_err()
        {
            return;
        }
        for (seq, event) in batch.events {
            if vexor_proto::send(&mut write, &FromShim::Event { seq, event })
                .await
                .is_err()
            {
                return;
            }
        }
        cursor = batch.next;

        tokio::select! {
            () = &mut changed => {}
            sampled = metrics.changed(), if send_metrics => {
                if sampled.is_err() {
                    send_metrics = false;
                    continue;
                }
                let samples = metrics.borrow_and_update().clone();
                if !samples.is_empty()
                    && vexor_proto::send(&mut write, &FromShim::Metrics { samples }).await.is_err()
                {
                    return;
                }
            }
            () = async { route_tick.as_mut().expect("guarded").tick().await; }, if route_tick.is_some() => {
                let tap = &routes.as_ref().expect("with the tick").0;
                if let Some(window) = tap.take(crate::output::now_ms())
                    && vexor_proto::send(&mut write, &FromShim::Routes { window }).await.is_err()
                {
                    return;
                }
            }
            // The connection closed (commands are carried out by the reader).
            msg = rx.recv() => if msg.is_none() { return },
        }
    }
}

struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock_is_exclusive_and_released_on_drop() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("a.lock");
        let held = try_lock(&path).expect("lock").expect("free");
        assert!(try_lock(&path).expect("lock").is_none());
        assert!(
            lock_with_retry(&path, Duration::from_millis(50))
                .expect("lock")
                .is_none()
        );
        drop(held);
        // Another test in this process may fork a child at this moment; the
        // child briefly shares the lock until it execs (CLOEXEC). Retry for
        // that window, as daemon and shim do at startup.
        assert!(lock_with_retry(&path, LOCK_WAIT).expect("lock").is_some());
    }

    #[test]
    fn retry_outlasts_a_brief_probe() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("a.lock");
        let probe = try_lock(&path).expect("lock").expect("free");
        let p = path.clone();
        let t = std::thread::spawn(move || lock_with_retry(&p, LOCK_WAIT).expect("lock"));
        std::thread::sleep(Duration::from_millis(30));
        drop(probe);
        assert!(t.join().expect("join").is_some());
    }

    #[test]
    fn atomic_write_replaces_contents() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("p.json");
        write_atomic(&path, b"one").expect("write");
        write_atomic(&path, b"two").expect("write");
        assert_eq!(std::fs::read(&path).expect("read"), b"two");
        let mode = std::fs::metadata(&path).expect("meta").permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}

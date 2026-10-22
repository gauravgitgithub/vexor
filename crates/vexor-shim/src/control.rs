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
use tokio::sync::{mpsc, watch};
use vexor_proto::{FromShim, PROTOCOL, ToShim};

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
    std::fs::rename(&tmp, path)
}

/// Accept daemon connections until the task is aborted.
pub async fn serve(
    listener: UnixListener,
    out: Output,
    me: Identity,
    app: Arc<str>,
    stop: Arc<watch::Sender<bool>>,
) {
    let mut current: Option<tokio::task::JoinHandle<()>> = None;
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
        if let Some(old) = current.take() {
            old.abort();
        }
        current = Some(tokio::spawn(session(
            stream,
            out.clone(),
            me,
            app.clone(),
            stop.clone(),
        )));
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

async fn session(
    stream: UnixStream,
    out: Output,
    me: Identity,
    app: Arc<str>,
    stop: Arc<watch::Sender<bool>>,
) {
    let (read, mut write) = stream.into_split();

    // Reading isn't cancel-safe (a partial line would be lost), so it runs
    // in its own task and hands over whole messages.
    let (tx, mut rx) = mpsc::channel::<ToShim>(8);
    let reader = tokio::spawn(async move {
        let mut r = BufReader::new(read);
        let mut buf = Vec::new();
        while let Ok(Some(msg)) = vexor_proto::recv::<_, ToShim>(&mut r, &mut buf).await {
            if tx.send(msg).await.is_err() {
                break;
            }
        }
    });
    let _guard = AbortOnDrop(reader);

    let resume = match tokio::time::timeout(HELLO_TIMEOUT, rx.recv()).await {
        Ok(Some(ToShim::Hello { protocol, resume })) if protocol == PROTOCOL => resume,
        Ok(Some(ToShim::Hello { protocol, .. })) => {
            let reason = format!("protocol {protocol} not supported, shim speaks {PROTOCOL}");
            let _ = vexor_proto::send(&mut write, &FromShim::Refused { reason }).await;
            return;
        }
        _ => return,
    };

    let next_seq = out.next_seq();
    let mut cursor = match resume {
        Some(r) if r.shim_start == me.start && r.last_seq < next_seq => r.last_seq + 1,
        // Fresh daemon: current snapshot, then new events only.
        None => next_seq,
        // The daemon followed an earlier shim: replay everything buffered.
        Some(_) => 0,
    };
    let welcome = FromShim::Welcome {
        protocol: PROTOCOL,
        app: app.to_string(),
        shim_pid: me.pid,
        shim_start: me.start,
        next_seq,
        snapshot: out.snapshot(),
    };
    if vexor_proto::send(&mut write, &welcome).await.is_err() {
        return;
    }

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
            msg = rx.recv() => match msg {
                Some(ToShim::Stop) => { stop.send_replace(true); }
                Some(ToShim::Hello { .. }) => {}
                None => return,
            },
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
        drop(held);
        assert!(try_lock(&path).expect("lock").is_some());
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

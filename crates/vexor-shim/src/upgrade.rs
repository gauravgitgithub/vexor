//! In-place upgrade (docs/architecture.md §7): the shim replaces its program
//! image with a newer `vexor` (`execve`) without touching the apps. The PID
//! stays, so the apps stay its children; their output pipes, IPC channels,
//! the control socket and the app lock are kept open across the exec, and
//! everything else the new image needs is written to a handover file.

use std::ffi::OsString;
use std::io;
use std::os::fd::{FromRawFd, IntoRawFd, OwnedFd};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::output::Saved;

/// Handover file format.
pub const FORMAT: u32 = 1;
/// Argument that tells `vexor shim` to resume from a handover file.
pub const ARG: &str = "--handover";

#[derive(Debug, Serialize, Deserialize)]
pub struct Handover {
    pub format: u32,
    /// Version of the image that wrote it.
    pub from_version: String,
    pub listener_fd: i32,
    pub lock_fd: i32,
    pub output: Saved,
    /// Instance count (it may differ from the spec after `vexor scale`).
    pub wanted: u32,
    pub instances: Vec<SavedInstance>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SavedInstance {
    pub index: u32,
    pub restarts: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process: Option<SavedProcess>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SavedProcess {
    pub pid: u32,
    pub start: u64,
    /// Adopted from a shim that died: not our child.
    #[serde(default)]
    pub adopted: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ipc_fd: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stdout_fd: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stderr_fd: Option<i32>,
}

pub fn path(shims_dir: &Path, app: &str) -> PathBuf {
    shims_dir.join(format!("{app}.handover.json"))
}

/// Keep `fd` open across `execve` (no close-on-exec) and give up ownership:
/// from now on it is known by its number, until [`take`].
pub fn keep(fd: OwnedFd) -> io::Result<i32> {
    rustix::io::fcntl_setfd(&fd, rustix::io::FdFlags::empty())?;
    Ok(fd.into_raw_fd())
}

/// Own again a descriptor [`keep`] gave up (in the new image, or in the old
/// one when the exec failed), closed again on a later exec.
#[allow(unsafe_code)]
pub fn take(fd: i32) -> io::Result<OwnedFd> {
    // SAFETY: only numbers recorded by `keep` in this process (or by the
    // image before the exec, which keeps the descriptor table) reach here,
    // each taken once, and the descriptor is checked to be open first.
    let borrowed = unsafe { std::os::fd::BorrowedFd::borrow_raw(fd) };
    rustix::io::fcntl_getfd(borrowed)?;
    // SAFETY: as above; ownership moves to the returned value.
    let owned = unsafe { OwnedFd::from_raw_fd(fd) };
    rustix::io::fcntl_setfd(&owned, rustix::io::FdFlags::CLOEXEC)?;
    Ok(owned)
}

pub fn write(path: &Path, h: &Handover) -> io::Result<()> {
    let json = serde_json::to_vec(h).map_err(io::Error::other)?;
    crate::control::write_atomic(path, &json)
}

/// Read and remove the handover file.
pub fn read(path: &Path) -> Result<Handover, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let _ = std::fs::remove_file(path);
    let h: Handover =
        serde_json::from_slice(&bytes).map_err(|e| format!("{}: {e}", path.display()))?;
    if h.format != FORMAT {
        return Err(format!(
            "{}: handover format {} is not {FORMAT}",
            path.display(),
            h.format
        ));
    }
    Ok(h)
}

/// Replace this program image with `exe`, same arguments plus
/// `--handover <file>`. Returns only if the exec failed.
pub fn exec(exe: &Path, handover: &Path) -> io::Error {
    use std::os::unix::process::CommandExt;
    let mut args: Vec<OsString> = std::env::args_os().skip(1).collect();
    if let Some(i) = args.iter().position(|a| a == ARG) {
        args.drain(i..(i + 2).min(args.len()));
    }
    std::process::Command::new(exe)
        .args(args)
        .arg(ARG)
        .arg(handover)
        .exec()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kept_descriptors_survive_and_are_taken_back() {
        let (a, b) = std::os::unix::net::UnixStream::pair().expect("pair");
        let n = keep(OwnedFd::from(a)).expect("keep");
        let flags = rustix::io::fcntl_getfd(unsafe_borrow(n)).expect("open");
        assert!(!flags.contains(rustix::io::FdFlags::CLOEXEC));
        let back = take(n).expect("take");
        let flags = rustix::io::fcntl_getfd(&back).expect("open");
        assert!(flags.contains(rustix::io::FdFlags::CLOEXEC));
        // Still the same socket: the other end hears it.
        let mut s = std::os::unix::net::UnixStream::from(back);
        std::io::Write::write_all(&mut s, b"x").expect("write");
        let mut buf = [0u8; 1];
        std::io::Read::read_exact(&mut &b, &mut buf).expect("read");
        assert_eq!(&buf, b"x");
        // A closed number is refused, not wrapped.
        drop(s);
        assert!(take(n).is_err());
    }

    #[allow(unsafe_code)]
    fn unsafe_borrow(fd: i32) -> std::os::fd::BorrowedFd<'static> {
        // SAFETY: test-only, the descriptor stays open for the check.
        unsafe { std::os::fd::BorrowedFd::borrow_raw(fd) }
    }

    #[test]
    fn handover_file_round_trips_and_is_removed() {
        let dir = tempfile::tempdir().expect("dir");
        let p = path(dir.path(), "web");
        let h = Handover {
            format: FORMAT,
            from_version: "0.12.0".into(),
            listener_fd: 5,
            lock_fd: 6,
            output: Saved::default(),
            wanted: 2,
            instances: vec![SavedInstance {
                index: 0,
                restarts: 3,
                process: Some(SavedProcess {
                    pid: 42,
                    start: 7,
                    adopted: false,
                    ipc_fd: None,
                    stdout_fd: Some(8),
                    stderr_fd: Some(9),
                }),
            }],
        };
        write(&p, &h).expect("write");
        let back = read(&p).expect("read");
        assert_eq!(
            back.instances[0].process.as_ref().map(|p| p.stdout_fd),
            Some(Some(8))
        );
        assert_eq!(back.wanted, 2);
        assert!(!p.exists());
        assert!(read(&p).is_err());
    }
}

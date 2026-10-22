//! Processes the app leaves behind.
//!
//! Two mechanisms, matching docs/architecture.md §2:
//! - Process group (all platforms): each instance leads its own group, so
//!   descendants that stay in the group are signalled with it.
//! - Child subreaper (Linux only): descendants that escape the group (e.g.
//!   `setsid`, double fork) re-parent to the shim instead of PID 1. The shim
//!   reaps them when they exit, reports them while alive, and stops them when
//!   the shim shuts down. On macOS they re-parent to launchd and are out of
//!   reach; that limitation is documented in docs/compatibility.md.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use crate::identity::Identity;

/// PIDs of instance leaders. They are the shim's own children, waited on by
/// tokio; orphan handling must never reap them.
#[derive(Debug, Clone, Default)]
pub struct Managed(Arc<Mutex<HashSet<u32>>>);

impl Managed {
    pub fn insert(&self, pid: u32) {
        self.lock().insert(pid);
    }

    pub fn remove(&self, pid: u32) {
        self.lock().remove(&pid);
    }

    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    fn contains(&self, pid: u32) -> bool {
        self.lock().contains(&pid)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashSet<u32>> {
        // The set stays consistent even if a holder panicked mid-update.
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// Ask the kernel to re-parent orphaned descendants to this process.
#[cfg(target_os = "linux")]
pub fn become_subreaper() -> std::io::Result<()> {
    rustix::process::set_child_subreaper(Some(rustix::process::getpid())).map_err(Into::into)
}

#[cfg(not(target_os = "linux"))]
pub fn become_subreaper() -> std::io::Result<()> {
    Ok(())
}

/// Reap exited orphans (adopted children that are not instance leaders).
/// Returns the identities of orphans that are still alive.
pub fn reap(managed: &Managed) -> Vec<Identity> {
    let mut alive = Vec::new();
    for child in adopted_children(managed) {
        if child.zombie {
            if let Some(pid) = to_pid(child.id.pid) {
                let _ = rustix::process::waitpid(Some(pid), rustix::process::WaitOptions::NOHANG);
            }
        } else {
            alive.push(child.id);
        }
    }
    alive
}

/// Stop all live orphans: SIGTERM, then SIGKILL after `grace`.
pub async fn stop_all(managed: &Managed, grace: std::time::Duration) {
    use rustix::process::Signal;

    let orphans = reap(managed);
    if orphans.is_empty() {
        return;
    }
    for id in &orphans {
        signal_if_same(id, Signal::TERM);
    }
    let deadline = tokio::time::Instant::now() + grace;
    while tokio::time::Instant::now() < deadline {
        if reap(managed).is_empty() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    for id in reap(managed) {
        signal_if_same(&id, Signal::KILL);
    }
    // Give the kernel a moment to deliver SIGKILL, then reap.
    for _ in 0..40 {
        if reap(managed).is_empty() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
}

fn signal_if_same(id: &Identity, sig: rustix::process::Signal) {
    // An adopted orphan is our child until reaped, so its PID can't be
    // reused; the identity check guards the window before it was adopted.
    if id.is_alive()
        && let Some(pid) = to_pid(id.pid)
    {
        let _ = rustix::process::kill_process(pid, sig);
    }
}

fn to_pid(pid: u32) -> Option<rustix::process::Pid> {
    rustix::process::Pid::from_raw(i32::try_from(pid).ok()?)
}

struct Adopted {
    id: Identity,
    zombie: bool,
}

#[cfg(target_os = "linux")]
fn adopted_children(managed: &Managed) -> Vec<Adopted> {
    let me = std::process::id();
    let Ok(dir) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in dir.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|s| s.parse::<u32>().ok())
        else {
            continue;
        };
        if managed.contains(pid) {
            continue;
        }
        let Ok(stat) = std::fs::read_to_string(entry.path().join("stat")) else {
            continue;
        };
        let Some(fields) = crate::identity::proc_stat_fields(&stat) else {
            continue;
        };
        if fields.ppid != me {
            continue;
        }
        out.push(Adopted {
            id: Identity {
                pid,
                start: fields.start,
            },
            zombie: fields.state == 'Z',
        });
    }
    out
}

#[cfg(not(target_os = "linux"))]
fn adopted_children(_managed: &Managed) -> Vec<Adopted> {
    Vec::new()
}

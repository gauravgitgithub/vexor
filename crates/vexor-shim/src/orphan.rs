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

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use crate::identity::Identity;

/// PIDs of instance leaders (and of the cluster primary). They are the
/// shim's own children, waited on by tokio; orphan handling must never reap
/// them. In daemon mode their identities are also persisted, so the daemon
/// can detect survivors of a shim crash and the next shim can adopt them.
#[derive(Debug, Clone, Default)]
pub struct Managed(Arc<Mutex<Leaders>>);

#[derive(Debug, Default)]
struct Leaders {
    pids: BTreeMap<u32, (Option<Identity>, Option<u32>)>,
    persist: Option<PathBuf>,
}

/// One recorded process: its identity and, for an instance leader, the
/// instance number (absent in files from before 0.13, and for a cluster
/// primary).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Leader {
    #[serde(flatten)]
    pub identity: Identity,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance: Option<u32>,
}

impl Managed {
    /// Record leaders in `path` from now on. Returns the leaders the
    /// previous shim recorded there that are still alive (it died without
    /// stopping them).
    pub fn persisted(path: PathBuf) -> (Self, Vec<Leader>) {
        let survivors: Vec<Leader> = std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice::<Vec<Leader>>(&b).ok())
            .unwrap_or_default()
            .into_iter()
            .filter(|l| l.identity.is_alive())
            .collect();
        let m = Managed::default();
        {
            let mut l = m.lock();
            l.persist = Some(path);
            // Still ours to account for until adopted or stopped.
            for s in &survivors {
                l.pids
                    .insert(s.identity.pid, (Some(s.identity), s.instance));
            }
        }
        m.save();
        (m, survivors)
    }

    /// A leader started (or adopted) for `instance`, or the cluster primary.
    pub fn insert(&self, pid: u32, identity: Option<Identity>, instance: Option<u32>) {
        self.lock().pids.insert(pid, (identity, instance));
        self.save();
    }

    pub fn remove(&self, pid: u32) {
        self.lock().pids.remove(&pid);
        self.save();
    }

    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    fn contains(&self, pid: u32) -> bool {
        self.lock().pids.contains_key(&pid)
    }

    fn save(&self) {
        let leaders = self.lock();
        let Some(path) = &leaders.persist else {
            return;
        };
        let ids: Vec<Leader> = leaders
            .pids
            .values()
            .filter_map(|&(identity, instance)| {
                Some(Leader {
                    identity: identity?,
                    instance,
                })
            })
            .collect();
        let result = serde_json::to_vec(&ids)
            .map_err(std::io::Error::other)
            .and_then(|json| crate::control::write_atomic(path, &json));
        if let Err(e) = result {
            eprintln!(
                "vexor shim: cannot record instance pids in {}: {e}",
                path.display()
            );
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Leaders> {
        // The map stays consistent even if a holder panicked mid-update.
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

/// Stop processes a dead shim left behind that won't be adopted: their
/// process groups get SIGTERM, then SIGKILL after `grace`. Identity-checked,
/// since they are not our children and their PIDs could be reused.
pub async fn stop_leftovers(leftovers: &[Identity], grace: std::time::Duration) {
    use rustix::process::Signal;

    let signal = |sig| {
        for id in leftovers {
            if id.is_alive()
                && let Some(pid) = to_pid(id.pid)
            {
                let _ = rustix::process::kill_process_group(pid, sig)
                    .or_else(|_| rustix::process::kill_process(pid, sig));
            }
        }
    };
    signal(Signal::TERM);
    let deadline = tokio::time::Instant::now() + grace;
    while tokio::time::Instant::now() < deadline {
        if !leftovers.iter().any(Identity::is_alive) {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    signal(Signal::KILL);
}

fn to_pid(pid: u32) -> Option<rustix::process::Pid> {
    rustix::process::Pid::from_raw(i32::try_from(pid).ok()?)
}

struct Adopted {
    id: Identity,
    zombie: bool,
}

/// PIDs of this process's children. Cheap: `/proc/self/task/*/children`
/// lists them directly. `None` when the kernel doesn't provide those files
/// (CONFIG_PROC_CHILDREN), in which case all of `/proc` must be scanned.
#[cfg(target_os = "linux")]
fn own_children() -> Option<Vec<u32>> {
    let tasks = std::fs::read_dir("/proc/self/task").ok()?;
    let mut out = Vec::new();
    let mut found = false;
    for t in tasks.flatten() {
        if let Ok(list) = std::fs::read_to_string(t.path().join("children")) {
            found = true;
            out.extend(
                list.split_whitespace()
                    .filter_map(|p| p.parse::<u32>().ok()),
            );
        }
    }
    found.then_some(out)
}

#[cfg(target_os = "linux")]
fn adopted_children(managed: &Managed) -> Vec<Adopted> {
    let me = std::process::id();
    let candidates: Vec<u32> = match own_children() {
        Some(c) => c,
        None => std::fs::read_dir("/proc")
            .map(|d| {
                d.flatten()
                    .filter_map(|e| e.file_name().to_str().and_then(|s| s.parse::<u32>().ok()))
                    .collect()
            })
            .unwrap_or_default(),
    };
    let mut out = Vec::new();
    for pid in candidates {
        if managed.contains(pid) {
            continue;
        }
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
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

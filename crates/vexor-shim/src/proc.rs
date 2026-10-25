//! An instance's process, however this shim came to hold it.

use std::time::Duration;

use vexor_core::ExitStatus;

use crate::identity::Identity;

/// How often a process that tokio doesn't wait for is checked.
const INHERITED_POLL: Duration = Duration::from_millis(50);
const ADOPTED_POLL: Duration = Duration::from_millis(250);

pub enum Proc {
    /// Started by this shim: tokio waits for it.
    Spawned(tokio::process::Child),
    /// Our child, started before an in-place upgrade (`execve` keeps
    /// children): reaped with `waitpid`. Listed in `Managed`, so the orphan
    /// reaper leaves it alone.
    Inherited(u32),
    /// Left running by a shim that died: not our child, so neither waited
    /// for nor reaped; it is gone once its identity no longer matches. Its
    /// exit status is unknown.
    Adopted(Identity),
}

impl Proc {
    /// Wait for the process to exit. Cancel-safe.
    pub async fn wait(&mut self) -> ExitStatus {
        match self {
            Proc::Spawned(c) => match c.wait().await {
                Ok(status) => {
                    use std::os::unix::process::ExitStatusExt;
                    ExitStatus {
                        code: status.code(),
                        signal: status.signal(),
                    }
                }
                Err(e) => {
                    eprintln!("vexor shim: wait failed: {e}");
                    ExitStatus::default()
                }
            },
            Proc::Inherited(pid) => {
                let Some(p) = i32::try_from(*pid)
                    .ok()
                    .and_then(rustix::process::Pid::from_raw)
                else {
                    return ExitStatus::default();
                };
                loop {
                    match rustix::process::waitpid(Some(p), rustix::process::WaitOptions::NOHANG) {
                        Ok(Some((_, status))) => {
                            return ExitStatus {
                                code: status.exit_status(),
                                signal: status.terminating_signal(),
                            };
                        }
                        Ok(None) => tokio::time::sleep(INHERITED_POLL).await,
                        // Not our child (any more): nothing to wait for.
                        Err(_) => return ExitStatus::default(),
                    }
                }
            }
            Proc::Adopted(identity) => {
                while identity.is_alive() {
                    tokio::time::sleep(ADOPTED_POLL).await;
                }
                ExitStatus::default()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn inherited_children_report_their_exit_status() {
        let child = std::process::Command::new("/bin/sh")
            .args(["-c", "exit 7"])
            .spawn()
            .expect("spawn");
        let mut p = Proc::Inherited(child.id());
        let status = tokio::time::timeout(Duration::from_secs(10), p.wait())
            .await
            .expect("exits");
        assert_eq!((status.code, status.signal), (Some(7), None));
        // `child` is never waited for by std: reaped above.
        drop(child);
    }

    #[tokio::test]
    async fn adopted_processes_are_followed_until_gone() {
        let mut child = std::process::Command::new("/bin/sleep")
            .arg("0.3")
            .spawn()
            .expect("spawn");
        let id = Identity::of(child.id()).expect("identity");
        // Reap it from another thread, as its real parent would.
        let reaper = std::thread::spawn(move || child.wait());
        let mut p = Proc::Adopted(id);
        let status = tokio::time::timeout(Duration::from_secs(10), p.wait())
            .await
            .expect("noticed");
        assert_eq!((status.code, status.signal), (None, None));
        reaper.join().expect("join").expect("wait");
    }
}

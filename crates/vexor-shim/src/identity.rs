//! Process identity: a PID plus the process start time, so a recycled PID is
//! never mistaken for the process Vexor started (docs/architecture.md §5).

use std::io;

use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
pub struct Identity {
    pub pid: u32,
    /// Opaque, platform-specific start time. Only compared for equality.
    pub start: u64,
}

impl Identity {
    /// Identity of the live process `pid`.
    pub fn of(pid: u32) -> io::Result<Identity> {
        Ok(Identity {
            pid,
            start: start_time(pid)?,
        })
    }

    /// True if `pid` is alive and is still the same process.
    pub fn is_alive(&self) -> bool {
        Identity::of(self.pid).is_ok_and(|now| now == *self)
    }
}

/// Field 22 of `/proc/<pid>/stat`: start time in clock ticks since boot.
#[cfg(target_os = "linux")]
fn start_time(pid: u32) -> io::Result<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat"))?;
    parse_proc_stat_start(&stat)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "malformed /proc stat"))
}

/// `proc_pidinfo(PROC_PIDTBSDINFO)` start time, in microseconds since the epoch.
#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
fn start_time(pid: u32) -> io::Result<u64> {
    let pid = i32::try_from(pid).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
    let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::zeroed();
    let size = i32::try_from(std::mem::size_of::<libc::proc_bsdinfo>())
        .map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
    // SAFETY: `info` is a writable buffer of exactly `size` bytes for the
    // struct this flavor fills in, and the kernel writes at most `size` bytes.
    let written = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            info.as_mut_ptr().cast(),
            size,
        )
    };
    if written != size {
        return Err(if written <= 0 {
            io::Error::last_os_error()
        } else {
            io::Error::new(io::ErrorKind::InvalidData, "short proc_pidinfo read")
        });
    }
    // SAFETY: the kernel filled all `size` bytes (checked above), and the
    // struct is plain integers, valid for any bit pattern (zeroed regardless).
    let info = unsafe { info.assume_init() };
    Ok(info
        .pbi_start_tvsec
        .saturating_mul(1_000_000)
        .saturating_add(info.pbi_start_tvusec))
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn start_time(_pid: u32) -> io::Result<u64> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "process identity is not implemented on this platform",
    ))
}

/// The command name in `/proc/<pid>/stat` is in parentheses and may itself
/// contain spaces and parentheses, so fields are counted from the last `)`.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn parse_proc_stat_start(stat: &str) -> Option<u64> {
    let rest = &stat[stat.rfind(')')? + 1..];
    // After ")": field 3 (state) is index 0, so field 22 is index 19.
    rest.split_whitespace().nth(19)?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn own_identity_is_stable_and_alive() {
        let me = Identity::of(std::process::id()).expect("own identity");
        assert_eq!(Identity::of(me.pid).expect("again"), me);
        assert!(me.is_alive());
    }

    #[test]
    fn different_start_time_is_not_alive() {
        let me = Identity::of(std::process::id()).expect("own identity");
        let impostor = Identity {
            start: me.start.wrapping_add(1),
            ..me
        };
        assert!(!impostor.is_alive());
    }

    #[test]
    fn exited_process_has_no_identity() {
        let mut child = std::process::Command::new("true")
            .spawn()
            .expect("spawn true");
        let pid = child.id();
        child.wait().expect("wait");
        assert!(Identity::of(pid).is_err());
    }

    #[test]
    fn proc_stat_with_hostile_command_name() {
        let stat = "1234 (evil) name (x) S 1 1234 1234 0 -1 4194560 100 0 0 0 1 2 0 0 20 0 1 0 98765 1000 100";
        assert_eq!(parse_proc_stat_start(stat), Some(98765));
        assert_eq!(parse_proc_stat_start("garbage"), None);
    }
}

//! Resident memory of a process, for `max_memory_restart`.

use std::io;

/// Resident set size of `pid`, in bytes.
#[cfg(target_os = "linux")]
pub fn rss_bytes(pid: u32) -> io::Result<u64> {
    // statm: size resident shared … (in pages).
    let statm = std::fs::read_to_string(format!("/proc/{pid}/statm"))?;
    let pages: u64 = statm
        .split_whitespace()
        .nth(1)
        .and_then(|f| f.parse().ok())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "malformed /proc statm"))?;
    Ok(pages.saturating_mul(rustix::param::page_size() as u64))
}

/// `proc_pidinfo(PROC_PIDTASKINFO)` resident size, in bytes.
#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
pub fn rss_bytes(pid: u32) -> io::Result<u64> {
    let pid = i32::try_from(pid).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
    let mut info = std::mem::MaybeUninit::<libc::proc_taskinfo>::zeroed();
    let size = i32::try_from(std::mem::size_of::<libc::proc_taskinfo>())
        .map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
    // SAFETY: `info` is a writable buffer of exactly `size` bytes for the
    // struct this flavor fills in, and the kernel writes at most `size` bytes.
    let written = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTASKINFO,
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
    Ok(info.pti_resident_size)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn rss_bytes(_pid: u32) -> io::Result<u64> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "memory measurement is not implemented on this platform",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn own_rss_is_plausible_and_grows() {
        let before = rss_bytes(std::process::id()).expect("rss");
        assert!(before > 100 * 1024, "{before}");
        // Touch 64 MiB so it becomes resident.
        let big = vec![1u8; 64 << 20];
        let after = rss_bytes(std::process::id()).expect("rss");
        assert!(after >= before + (48 << 20), "{before} -> {after}");
        drop(big);
    }

    #[test]
    fn missing_process_is_an_error() {
        let mut child = std::process::Command::new("true").spawn().expect("spawn");
        let pid = child.id();
        child.wait().expect("wait");
        assert!(rss_bytes(pid).is_err());
    }
}

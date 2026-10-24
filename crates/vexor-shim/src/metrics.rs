//! Resource sampling for `list`/`describe` (docs/plan.md 6b.1): every
//! `metrics_interval_ms` the shim measures each running instance's CPU,
//! resident memory and open files, and hands the latest sample to the
//! daemon connection. Only the instance leader is measured, not processes
//! it spawned (as PM2).

use std::collections::HashMap;
use std::io;
use std::time::Duration;

use tokio::sync::watch;
use tokio::time::Instant;
use vexor_proto::InstanceMetrics;

use crate::output::{Output, now_ms};

/// One reading of a live process.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Usage {
    /// Identity start time (see [`Identity`]), so a recycled PID is never
    /// measured as the process the shim started.
    pub start: u64,
    /// Total CPU time used, user + system.
    pub cpu: Duration,
    pub rss_bytes: u64,
    pub open_fds: Option<u64>,
}

/// Sample every running instance each `interval` and publish the result.
/// Runs until aborted.
pub async fn run(out: Output, interval: Duration, publish: watch::Sender<Vec<InstanceMetrics>>) {
    let mut tick = tokio::time::interval(interval);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // Previous reading per instance: (pid, usage, when).
    let mut previous: HashMap<u32, (u32, Usage, Instant)> = HashMap::new();
    loop {
        tick.tick().await;
        let mut samples = Vec::new();
        let mut seen = HashMap::new();
        for s in out.snapshot() {
            let (Some(instance), Some(pid), Some(start)) = (
                s["instance"].as_u64().and_then(|i| u32::try_from(i).ok()),
                s["pid"].as_u64().and_then(|p| u32::try_from(p).ok()),
                s["start"].as_u64(),
            ) else {
                continue;
            };
            let Ok(usage) = usage(pid) else {
                continue;
            };
            if usage.start != start {
                continue;
            }
            let now = Instant::now();
            let cpu_pct = previous
                .get(&instance)
                .filter(|(p, u, _)| *p == pid && u.start == start)
                .and_then(|(_, u, at)| cpu_pct(u.cpu, usage.cpu, now - *at));
            seen.insert(instance, (pid, usage, now));
            samples.push(InstanceMetrics {
                instance,
                pid,
                ts_ms: now_ms(),
                cpu_pct,
                rss_bytes: usage.rss_bytes,
                open_fds: usage.open_fds,
                health: out.health(instance),
            });
        }
        previous = seen;
        publish.send_replace(samples);
    }
}

/// CPU used between two readings, in % of one core (rounded to 0.1).
pub fn cpu_pct(before: Duration, after: Duration, elapsed: Duration) -> Option<f64> {
    if elapsed.is_zero() {
        return None;
    }
    let used = after.saturating_sub(before).as_secs_f64();
    Some((used / elapsed.as_secs_f64() * 1000.0).round() / 10.0)
}

#[cfg(target_os = "linux")]
pub fn usage(pid: u32) -> io::Result<Usage> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat"))?;
    let f = crate::identity::proc_stat_fields(&stat)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "malformed /proc stat"))?;
    let hz = rustix::param::clock_ticks_per_second().max(1);
    let ticks = f.utime.saturating_add(f.stime);
    let cpu =
        Duration::from_secs(ticks / hz) + Duration::from_nanos(ticks % hz * 1_000_000_000 / hz);
    let open_fds = std::fs::read_dir(format!("/proc/{pid}/fd"))
        .ok()
        .map(|d| d.count() as u64);
    Ok(Usage {
        start: f.start,
        cpu,
        rss_bytes: crate::memory::rss_bytes(pid)?,
        open_fds,
    })
}

#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
pub fn usage(pid: u32) -> io::Result<Usage> {
    let start = crate::identity::Identity::of(pid)?.start;
    let raw = i32::try_from(pid).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
    let mut info = std::mem::MaybeUninit::<libc::proc_taskinfo>::zeroed();
    let size = i32::try_from(std::mem::size_of::<libc::proc_taskinfo>())
        .map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
    // SAFETY: `info` is a writable buffer of exactly `size` bytes for the
    // struct this flavor fills in, and the kernel writes at most `size` bytes.
    let written = unsafe {
        libc::proc_pidinfo(
            raw,
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
    let (numer, denom) = timebase();
    // Task times are in Mach absolute time units (ns on Intel, not on arm64).
    let ticks = u128::from(info.pti_total_user.saturating_add(info.pti_total_system));
    let ns = ticks * u128::from(numer) / u128::from(denom.max(1));
    Ok(Usage {
        start,
        cpu: Duration::from_nanos(u64::try_from(ns).unwrap_or(u64::MAX)),
        rss_bytes: info.pti_resident_size,
        open_fds: open_fds(raw),
    })
}

#[cfg(target_os = "macos")]
#[allow(unsafe_code, deprecated)]
fn timebase() -> (u32, u32) {
    static TB: std::sync::OnceLock<(u32, u32)> = std::sync::OnceLock::new();
    *TB.get_or_init(|| {
        let mut tb = libc::mach_timebase_info { numer: 1, denom: 1 };
        // SAFETY: writes one struct into `tb`, which is valid and writable.
        // Deprecated in libc only in favour of the `mach2` crate.
        unsafe { libc::mach_timebase_info(&mut tb) };
        (tb.numer, tb.denom)
    })
}

/// `PROC_PIDLISTFDS`. The size a NULL buffer reports can be short of the
/// real count, so the list is fetched into a buffer grown until it fits.
#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
fn open_fds(pid: i32) -> Option<u64> {
    let entry = std::mem::size_of::<libc::proc_fdinfo>();
    // SAFETY: a NULL buffer of size 0 asks only for a size estimate.
    let hint =
        unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDLISTFDS, 0, std::ptr::null_mut(), 0) };
    let mut cap = usize::try_from(hint).ok()? / entry + 64;
    loop {
        let mut buf = vec![std::mem::MaybeUninit::<libc::proc_fdinfo>::zeroed(); cap];
        let size = i32::try_from(cap * entry).ok()?;
        // SAFETY: `buf` is writable for `size` bytes; the kernel writes at most that.
        let written = unsafe {
            libc::proc_pidinfo(pid, libc::PROC_PIDLISTFDS, 0, buf.as_mut_ptr().cast(), size)
        };
        let count = usize::try_from(written).ok().filter(|w| *w > 0)? / entry;
        if count < cap {
            return Some(count as u64);
        }
        cap = cap.checked_mul(2)?;
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn usage(_pid: u32) -> io::Result<Usage> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "resource sampling is not implemented on this platform",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_percent_of_one_core() {
        let s = Duration::from_secs;
        assert_eq!(cpu_pct(s(1), s(3), s(4)), Some(50.0));
        assert_eq!(cpu_pct(s(0), s(10), s(5)), Some(200.0));
        assert_eq!(cpu_pct(s(3), s(1), s(4)), Some(0.0));
        assert_eq!(cpu_pct(s(1), s(2), Duration::ZERO), None);
    }

    #[test]
    fn own_usage_tracks_cpu() {
        let me = std::process::id();
        let before = usage(me).expect("usage");
        assert_eq!(
            before.start,
            crate::identity::Identity::of(me).expect("identity").start
        );
        assert!(before.rss_bytes > 100 * 1024, "{before:?}");
        let spin = std::time::Instant::now();
        let mut x = 0u64;
        while spin.elapsed() < Duration::from_millis(200) {
            x = std::hint::black_box(x.wrapping_add(1));
        }
        let after = usage(me).expect("usage");
        assert!(
            after.cpu >= before.cpu + Duration::from_millis(100),
            "{before:?} -> {after:?}"
        );
    }

    #[test]
    fn counts_open_files() {
        // Children, because other tests in this process open and close
        // files meanwhile.
        let spawn = |script: &str| {
            std::process::Command::new("/bin/sh")
                .args(["-c", script])
                .spawn()
                .expect("spawn")
        };
        let mut plain = spawn("exec sleep 30");
        let mut more = spawn(
            "exec 3</dev/null 4</dev/null 5</dev/null 6</dev/null 7</dev/null \
             8</dev/null 9</dev/null; exec 10</dev/null 11</dev/null 12</dev/null; exec sleep 30",
        );
        let fds = |pid| usage(pid).expect("usage").open_fds.expect("fds");
        // Wait until both have exec'd sleep (`sh` may hold extra files).
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let (p, m) = loop {
            std::thread::sleep(Duration::from_millis(50));
            let (p, m) = (fds(plain.id()), fds(more.id()));
            if m == p + 10 || std::time::Instant::now() > deadline {
                break (p, m);
            }
        };
        let _ = (plain.kill(), more.kill(), plain.wait(), more.wait());
        assert_eq!(m, p + 10, "{p} vs {m}");
    }

    #[test]
    fn exited_process_has_no_usage() {
        let mut child = std::process::Command::new("true").spawn().expect("spawn");
        let pid = child.id();
        child.wait().expect("wait");
        assert!(usage(pid).is_err());
    }
}

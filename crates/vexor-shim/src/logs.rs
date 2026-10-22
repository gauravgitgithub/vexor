//! App output to log files (docs/architecture.md §10).
//!
//! One [`Sink`] per log file, shared by all instances (and by stdout and
//! stderr when both point at the same file). Pipe readers push chunks into a
//! bounded queue; a dedicated writer thread does all disk I/O, so a slow or
//! failing disk never stalls the shim's runtime or restart loop.
//!
//! - Memory is bounded by the queue: `buffer_bytes` per file.
//! - `Overflow::Drop`: excess output is discarded and counted; a marker line
//!   goes into the log and a `log_dropped` event is emitted. The app never
//!   waits for the disk.
//! - `Overflow::Block`: readers wait for room, so the app blocks on its own
//!   writes. Nothing is lost.
//! - Size-based rotation: `out.log` → `out.log.1` → … → `out.log.N`.
//! - A file moved or deleted by an external tool (logrotate) is noticed within
//!   [`REOPEN_CHECK`] and reopened at its path.
//! - Write errors (disk full, …) are reported once per streak; the app keeps
//!   running and writing resumes when the disk recovers.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::sync::mpsc;
use vexor_core::{LogSpec, Overflow};

use crate::output::{Output, ShimEvent};

/// Bytes read from a pipe per chunk.
pub const CHUNK: usize = 8 * 1024;
/// How often the writer checks whether its file was moved or deleted.
const REOPEN_CHECK: Duration = Duration::from_secs(1);
/// Minimum time between two `log_dropped` events for one file.
const DROP_EVENT_INTERVAL: Duration = Duration::from_secs(10);

/// The log files of one app, by path.
#[derive(Debug)]
pub struct Logs {
    pub out: Sink,
    pub err: Sink,
    writers: Vec<std::thread::JoinHandle<()>>,
}

impl Logs {
    pub fn start(app: &str, spec: &LogSpec, events: &Output) -> io::Result<Logs> {
        let (out, w1) = Sink::start(app, &spec.out, spec, events)?;
        let mut writers = vec![w1];
        let err = if spec.err == spec.out {
            out.clone()
        } else {
            let (err, w2) = Sink::start(app, &spec.err, spec, events)?;
            writers.push(w2);
            err
        };
        Ok(Logs { out, err, writers })
    }

    /// Wait (up to `timeout`) for the writers to flush everything queued.
    /// Every sender must have been dropped first, or this times out.
    pub async fn finish(self, timeout: Duration) {
        let Logs { out, err, writers } = self;
        drop(out);
        drop(err);
        let joined = tokio::task::spawn_blocking(move || {
            for w in writers {
                let _ = w.join();
            }
        });
        let _ = tokio::time::timeout(timeout, joined).await;
    }
}

/// Producer side of one log file.
#[derive(Debug, Clone)]
pub struct Sink {
    tx: mpsc::Sender<Vec<u8>>,
    overflow: Overflow,
    dropped: Arc<AtomicU64>,
}

impl Sink {
    fn start(
        app: &str,
        path: &Path,
        spec: &LogSpec,
        events: &Output,
    ) -> io::Result<(Sink, std::thread::JoinHandle<()>)> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let file = LogFile::new(path.to_path_buf(), spec.max_size_bytes, spec.retain);
        let slots = (usize::try_from(spec.buffer_bytes).unwrap_or(usize::MAX) / CHUNK).max(1);
        let (tx, rx) = mpsc::channel(slots);
        let dropped = Arc::new(AtomicU64::new(0));
        let reporter = Reporter {
            app: app.to_owned(),
            path: path.to_path_buf(),
            events: events.clone(),
            last_drop_event: None,
            error_reported: false,
        };
        let d = dropped.clone();
        let handle = std::thread::Builder::new()
            .name("vexor-log".into())
            .spawn(move || writer(rx, file, &d, reporter))?;
        Ok((
            Sink {
                tx,
                overflow: spec.overflow,
                dropped,
            },
            handle,
        ))
    }

    /// Copy a pipe into this sink until EOF.
    pub async fn pump(self, mut from: impl AsyncRead + Unpin) {
        loop {
            let mut buf = vec![0; CHUNK];
            let n = match from.read(&mut buf).await {
                Ok(0) | Err(_) => return,
                Ok(n) => n,
            };
            buf.truncate(n);
            match self.overflow {
                Overflow::Block => {
                    if self.tx.send(buf).await.is_err() {
                        return;
                    }
                }
                Overflow::Drop => match self.tx.try_send(buf) {
                    Ok(()) => {}
                    Err(mpsc::error::TrySendError::Full(b)) => {
                        self.dropped.fetch_add(len_u64(&b), Ordering::Relaxed);
                    }
                    Err(mpsc::error::TrySendError::Closed(_)) => return,
                },
            }
        }
    }
}

fn len_u64(b: &[u8]) -> u64 {
    u64::try_from(b.len()).unwrap_or(u64::MAX)
}

/// Reports drops and write errors as shim events, without flooding.
struct Reporter {
    app: String,
    path: PathBuf,
    events: Output,
    last_drop_event: Option<Instant>,
    /// A write error was reported and no write has succeeded since.
    error_reported: bool,
}

impl Reporter {
    fn dropped(&mut self, bytes: u64, pending: &mut u64) {
        *pending += bytes;
        let due = self
            .last_drop_event
            .is_none_or(|t| t.elapsed() >= DROP_EVENT_INTERVAL);
        if due && *pending > 0 {
            self.events.emit(&ShimEvent::LogDropped {
                app: &self.app,
                path: &self.path,
                bytes: *pending,
            });
            *pending = 0;
            self.last_drop_event = Some(Instant::now());
        }
    }

    fn write_failed(&mut self, e: &io::Error) {
        if !self.error_reported {
            self.error_reported = true;
            self.events.emit(&ShimEvent::LogError {
                app: &self.app,
                path: &self.path,
                error: e.to_string(),
            });
        }
    }

    fn write_ok(&mut self) {
        self.error_reported = false;
    }
}

fn writer(
    mut rx: mpsc::Receiver<Vec<u8>>,
    mut file: LogFile,
    dropped: &AtomicU64,
    mut reporter: Reporter,
) {
    let mut unreported = 0;
    while let Some(chunk) = rx.blocking_recv() {
        let lost = dropped.swap(0, Ordering::Relaxed);
        if lost > 0 {
            let marker = format!(
                "\n[vexor] dropped {lost} bytes of output: it arrived faster than it could be written\n"
            );
            let _ = file.write(marker.as_bytes());
            reporter.dropped(lost, &mut unreported);
        }
        match file.write(&chunk) {
            Ok(()) => reporter.write_ok(),
            Err(e) => {
                reporter.write_failed(&e);
                reporter.dropped(len_u64(&chunk), &mut unreported);
            }
        }
    }
    // Senders gone: account for anything dropped since the last chunk.
    let lost = dropped.swap(0, Ordering::Relaxed);
    if lost > 0 {
        let marker = format!("\n[vexor] dropped {lost} bytes of output\n");
        let _ = file.write(marker.as_bytes());
        reporter.last_drop_event = None;
        reporter.dropped(lost, &mut unreported);
    } else if unreported > 0 {
        reporter.last_drop_event = None;
        reporter.dropped(0, &mut unreported);
    }
}

/// A log file with size-based rotation.
pub struct LogFile {
    path: PathBuf,
    max_size: u64,
    retain: u32,
    file: Option<File>,
    size: u64,
    /// (dev, ino) of the open file, to notice external moves.
    id: (u64, u64),
    last_check: Instant,
}

impl LogFile {
    pub fn new(path: PathBuf, max_size: u64, retain: u32) -> LogFile {
        LogFile {
            path,
            max_size,
            retain,
            file: None,
            size: 0,
            id: (0, 0),
            last_check: Instant::now(),
        }
    }

    pub fn write(&mut self, buf: &[u8]) -> io::Result<()> {
        self.check_moved();
        if self.file.is_none() {
            self.open()?;
        }
        let len = len_u64(buf);
        if self.max_size > 0 && self.size > 0 && self.size + len > self.max_size {
            self.rotate()?;
            self.open()?;
        }
        let file = self
            .file
            .as_mut()
            .ok_or_else(|| io::Error::other("log file not open"))?;
        match file.write_all(buf) {
            Ok(()) => {
                self.size += len;
                Ok(())
            }
            Err(e) => {
                // Reopen next time; the file may have been replaced.
                self.file = None;
                Err(e)
            }
        }
    }

    fn open(&mut self) -> io::Result<()> {
        let f = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o640)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&self.path)?;
        let meta = f.metadata()?;
        self.size = meta.len();
        self.id = (meta.dev(), meta.ino());
        self.file = Some(f);
        self.last_check = Instant::now();
        Ok(())
    }

    /// If the path no longer names the open file (moved, deleted, replaced),
    /// reopen at the path on the next write.
    fn check_moved(&mut self) {
        if self.file.is_none() || self.last_check.elapsed() < REOPEN_CHECK {
            return;
        }
        self.last_check = Instant::now();
        let same =
            std::fs::symlink_metadata(&self.path).is_ok_and(|m| (m.dev(), m.ino()) == self.id);
        if !same {
            self.file = None;
        }
    }

    fn rotate(&mut self) -> io::Result<()> {
        self.file = None;
        if self.retain == 0 {
            return match std::fs::remove_file(&self.path) {
                Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
                _ => Ok(()),
            };
        }
        let numbered = |i: u32| PathBuf::from(format!("{}.{i}", self.path.display()));
        for i in (1..self.retain).rev() {
            match std::fs::rename(numbered(i), numbered(i + 1)) {
                Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
                _ => {}
            }
        }
        std::fs::rename(&self.path, numbered(1))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(p: &Path) -> String {
        std::fs::read_to_string(p).unwrap_or_default()
    }

    #[test]
    fn rotates_by_size_and_keeps_retain_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("out.log");
        let mut f = LogFile::new(path.clone(), 10, 2);
        for i in 0..6 {
            f.write(format!("line{i}\n").as_bytes()).expect("write"); // 6 bytes each
        }
        // 10-byte cap: one line per file; newest in out.log.
        assert_eq!(read(&path), "line5\n");
        assert_eq!(read(&dir.path().join("out.log.1")), "line4\n");
        assert_eq!(read(&dir.path().join("out.log.2")), "line3\n");
        assert!(!dir.path().join("out.log.3").exists());
    }

    #[test]
    fn retain_zero_truncates() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("out.log");
        let mut f = LogFile::new(path.clone(), 10, 0);
        for i in 0..3 {
            f.write(format!("line{i}\n").as_bytes()).expect("write");
        }
        assert_eq!(read(&path), "line2\n");
        assert_eq!(std::fs::read_dir(dir.path()).expect("dir").count(), 1);
    }

    #[test]
    fn reopens_after_external_move() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("out.log");
        let mut f = LogFile::new(path.clone(), 0, 0);
        f.write(b"before\n").expect("write");
        std::fs::rename(&path, dir.path().join("moved.log")).expect("move");
        f.last_check = Instant::now() - REOPEN_CHECK;
        f.write(b"after\n").expect("write");
        assert_eq!(read(&path), "after\n");
        assert_eq!(read(&dir.path().join("moved.log")), "before\n");
    }

    #[test]
    fn refuses_symlinked_log_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("target");
        std::fs::write(&target, b"").expect("target");
        let link = dir.path().join("out.log");
        std::os::unix::fs::symlink(&target, &link).expect("symlink");
        let mut f = LogFile::new(link, 0, 0);
        assert!(f.write(b"x").is_err());
        assert_eq!(read(&target), "");
    }

    #[tokio::test]
    async fn drop_mode_counts_and_marks_lost_output() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("out.log");
        let spec = LogSpec {
            buffer_bytes: u32::try_from(CHUNK).expect("fits"),
            ..LogSpec::new(path.clone(), path.clone())
        };
        let events = Output::new(false);
        // A FIFO with no reader yet: the writer thread blocks opening it,
        // so the queue fills and further output is dropped.
        let fifo = dir.path().join("fifo.log");
        let status = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .expect("mkfifo");
        assert!(status.success());
        let spec = LogSpec {
            out: fifo.clone(),
            err: fifo.clone(),
            max_size_bytes: 0,
            ..spec
        };
        let logs = Logs::start("t", &spec, &events).expect("start");
        let input = vec![b'x'; CHUNK * 20];
        logs.out.clone().pump(&input[..]).await;
        // Now drain the FIFO.
        let reader = std::thread::spawn(move || std::fs::read(&fifo).expect("read fifo"));
        logs.finish(Duration::from_secs(5)).await;
        let got = reader.join().expect("reader");
        let text = String::from_utf8_lossy(&got);
        // Drops can be reported in more than one marker line.
        let lost: usize = text
            .lines()
            .filter(|l| l.starts_with("[vexor] dropped"))
            .map(|l| {
                l.split_whitespace()
                    .nth(2)
                    .and_then(|n| n.parse::<usize>().ok())
                    .expect("count")
            })
            .sum();
        // App output only: the marker line itself contains an `x` ("vexor").
        let kept: usize = text
            .lines()
            .filter(|l| !l.starts_with("[vexor]"))
            .map(|l| l.bytes().filter(|b| *b == b'x').count())
            .sum();
        assert_eq!(kept + lost, input.len(), "every byte is written or counted");
        assert!(lost > 0);
        let reported: u64 = events
            .since(0)
            .events
            .iter()
            .filter(|(_, e)| e["type"] == "log_dropped")
            .filter_map(|(_, e)| e["bytes"].as_u64())
            .sum();
        assert_eq!(reported, u64::try_from(lost).expect("fits"));
    }

    #[tokio::test]
    async fn block_mode_loses_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("out.log");
        let spec = LogSpec {
            buffer_bytes: u32::try_from(CHUNK).expect("fits"),
            overflow: Overflow::Block,
            max_size_bytes: 100_000,
            retain: 1000,
            ..LogSpec::new(path.clone(), path.clone())
        };
        let events = Output::new(false);
        let logs = Logs::start("t", &spec, &events).expect("start");
        let input: Vec<u8> = (0..200_000u32)
            .flat_map(|i| format!("{i}\n").into_bytes())
            .collect();
        logs.out.clone().pump(&input[..]).await;
        logs.finish(Duration::from_secs(10)).await;
        // Reassemble oldest to newest.
        let mut parts: Vec<(u32, PathBuf)> = std::fs::read_dir(dir.path())
            .expect("dir")
            .flatten()
            .map(|e| e.path())
            .map(|p| {
                let n = p
                    .extension()
                    .and_then(|e| e.to_str())
                    .and_then(|e| e.parse().ok())
                    .unwrap_or(0);
                (n, p)
            })
            .collect();
        parts.sort_by_key(|p| std::cmp::Reverse(p.0));
        let mut all = Vec::new();
        for (_, p) in parts {
            all.extend(std::fs::read(p).expect("read"));
        }
        assert_eq!(all, input);
    }
}

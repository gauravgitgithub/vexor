//! `vexor logs`: show and follow app log files.
//!
//! The daemon tells the CLI where each app's logs are; the CLI reads the
//! files itself (they belong to the same user). Rotated files (`out.log.1`)
//! fill in when the current file has fewer lines than asked for, and
//! `--follow` keeps going across rotation and truncation.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// One log file to show, with the label printed before its lines.
#[derive(Debug, Clone)]
pub struct Source {
    pub label: String,
    pub path: PathBuf,
}

const CHUNK: u64 = 64 * 1024;
const POLL: Duration = Duration::from_millis(200);

/// The last `n` lines of `path`, oldest first. Reads backwards in chunks,
/// so large files are not loaded whole.
pub fn last_lines(path: &Path, n: usize) -> io::Result<Vec<String>> {
    if n == 0 {
        return Ok(Vec::new());
    }
    let mut f = File::open(path)?;
    let len = f.metadata()?.len();
    let mut pos = len;
    let mut buf: Vec<u8> = Vec::new();
    loop {
        // More than n newlines guarantees n complete lines after the
        // (possibly partial) first one.
        let newlines = buf.iter().filter(|&&b| b == b'\n').count();
        if pos == 0 || newlines > n {
            break;
        }
        let read = CHUNK.min(pos);
        pos -= read;
        f.seek(SeekFrom::Start(pos))?;
        let mut chunk = vec![0; usize::try_from(read).unwrap_or(0)];
        f.read_exact(&mut chunk)?;
        chunk.extend_from_slice(&buf);
        buf = chunk;
    }
    let text = String::from_utf8_lossy(&buf);
    let lines: Vec<&str> = text.lines().collect();
    // Without reaching the start of the file, the first line may be partial.
    let complete = if pos > 0 {
        &lines[1.min(lines.len())..]
    } else {
        &lines[..]
    };
    let start = complete.len().saturating_sub(n);
    Ok(complete[start..].iter().map(|s| (*s).to_owned()).collect())
}

/// The last `n` lines across the current file and, if it is short, its
/// most recent rotated file.
pub fn last_lines_with_rotated(path: &Path, n: usize) -> Vec<String> {
    let mut lines = last_lines(path, n).unwrap_or_default();
    if lines.len() < n {
        let rotated = PathBuf::from(format!("{}.1", path.display()));
        if let Ok(mut older) = last_lines(&rotated, n - lines.len()) {
            older.append(&mut lines);
            lines = older;
        }
    }
    lines
}

fn print_line(out: &mut impl Write, label: &str, line: &str, raw: bool) -> io::Result<()> {
    if raw {
        writeln!(out, "{line}")
    } else {
        writeln!(out, "{label} | {line}")
    }
}

/// Print the last `n` lines of each source, one source after another.
pub fn print_tail(sources: &[Source], n: usize, raw: bool) -> io::Result<()> {
    let mut out = io::stdout().lock();
    for s in sources {
        if !raw {
            writeln!(
                out,
                "--- {} ({}), last {n} lines ---",
                s.label,
                s.path.display()
            )?;
        }
        for line in last_lines_with_rotated(&s.path, n) {
            print_line(&mut out, &s.label, &line, raw)?;
        }
    }
    out.flush()
}

struct Followed {
    source: Source,
    file: Option<File>,
    id: (u64, u64),
    offset: u64,
    partial: Vec<u8>,
}

impl Followed {
    fn new(source: Source) -> Followed {
        let mut f = Followed {
            source,
            file: None,
            id: (0, 0),
            offset: 0,
            partial: Vec::new(),
        };
        // Start at the end: only new lines are followed.
        if f.open() {
            f.offset = f
                .file
                .as_ref()
                .and_then(|x| x.metadata().ok())
                .map_or(0, |m| m.len());
        }
        f
    }

    fn open(&mut self) -> bool {
        match File::open(&self.source.path) {
            Ok(file) => {
                let meta = file.metadata().ok();
                self.id = meta.map_or((0, 0), |m| (m.dev(), m.ino()));
                self.file = Some(file);
                self.offset = 0;
                true
            }
            Err(_) => false,
        }
    }

    /// New complete lines since the last call.
    fn poll(&mut self) -> Vec<String> {
        // Rotated (path now names another file) or truncated: start over.
        let at_path = std::fs::metadata(&self.source.path).ok();
        let replaced = at_path
            .as_ref()
            .is_some_and(|m| (m.dev(), m.ino()) != self.id);
        if self.file.is_none() || replaced {
            if replaced {
                // Read what the old file still had before switching.
                let mut tail = self.read_new();
                if self.open() {
                    tail.extend(self.read_new());
                }
                return tail;
            }
            if !self.open() {
                return Vec::new();
            }
        }
        if at_path.is_some_and(|m| m.len() < self.offset) {
            self.offset = 0;
            self.partial.clear();
        }
        self.read_new()
    }

    fn read_new(&mut self) -> Vec<String> {
        let Some(file) = self.file.as_mut() else {
            return Vec::new();
        };
        if file.seek(SeekFrom::Start(self.offset)).is_err() {
            return Vec::new();
        }
        let mut buf = Vec::new();
        if let Ok(n) = file.read_to_end(&mut buf) {
            self.offset += u64::try_from(n).unwrap_or(0);
        }
        self.partial.extend_from_slice(&buf);
        let mut lines = Vec::new();
        while let Some(i) = self.partial.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.partial.drain(..=i).collect();
            lines.push(String::from_utf8_lossy(&line[..line.len() - 1]).into_owned());
        }
        lines
    }
}

/// Print new lines from every source as they arrive, until interrupted.
pub fn follow(sources: Vec<Source>, raw: bool) -> io::Result<()> {
    let mut followed: Vec<Followed> = sources.into_iter().map(Followed::new).collect();
    loop {
        let mut out = io::stdout().lock();
        for f in &mut followed {
            for line in f.poll() {
                print_line(&mut out, &f.source.label, &line, raw)?;
            }
        }
        out.flush()?;
        drop(out);
        std::thread::sleep(POLL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, name: &str, body: &str) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, body).expect("write");
        p
    }

    #[test]
    fn last_lines_small_and_large_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        let small = write(dir.path(), "s.log", "a\nb\nc\n");
        assert_eq!(last_lines(&small, 2).expect("read"), ["b", "c"]);
        assert_eq!(last_lines(&small, 10).expect("read"), ["a", "b", "c"]);
        assert!(last_lines(&small, 0).expect("read").is_empty());
        let no_newline = write(dir.path(), "n.log", "a\nb");
        assert_eq!(last_lines(&no_newline, 1).expect("read"), ["b"]);

        // Larger than one chunk: lines must come out whole and in order.
        let body: String = (0..50_000).map(|i| format!("line {i}\n")).collect();
        let big = write(dir.path(), "big.log", &body);
        let tail = last_lines(&big, 3).expect("read");
        assert_eq!(tail, ["line 49997", "line 49998", "line 49999"]);
        let many = last_lines(&big, 20_000).expect("read");
        assert_eq!(many.len(), 20_000);
        assert_eq!(many[0], "line 30000");
    }

    #[test]
    fn rotated_file_fills_in_short_current_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(dir.path(), "out.log.1", "old1\nold2\nold3\n");
        let cur = write(dir.path(), "out.log", "new1\n");
        assert_eq!(last_lines_with_rotated(&cur, 3), ["old2", "old3", "new1"]);
    }

    #[test]
    fn follow_sees_appends_truncation_and_rotation() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write(dir.path(), "out.log", "before\n");
        let mut f = Followed::new(Source {
            label: "a|out".into(),
            path: path.clone(),
        });
        assert!(f.poll().is_empty(), "starts at the end");
        let append = |s: &str| {
            use std::io::Write;
            std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .expect("open")
                .write_all(s.as_bytes())
                .expect("write");
        };
        append("one\ntw");
        assert_eq!(f.poll(), ["one"]);
        append("o\n");
        assert_eq!(f.poll(), ["two"], "partial lines are joined");
        // Rotation: old file moved away (with a last line), new file created.
        append("last-old\n");
        std::fs::rename(&path, dir.path().join("out.log.1")).expect("rotate");
        std::fs::write(&path, "first-new\n").expect("new file");
        assert_eq!(f.poll(), ["last-old", "first-new"]);
        // Truncation.
        std::fs::write(&path, "").expect("truncate");
        assert!(f.poll().is_empty());
        append("after-truncate\n");
        assert_eq!(f.poll(), ["after-truncate"]);
    }
}

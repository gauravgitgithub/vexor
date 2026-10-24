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

/// One log file to show.
#[derive(Debug, Clone)]
pub struct Source {
    pub app: String,
    /// Standard error (or a merged file, when `merged`).
    pub err: bool,
    pub merged: bool,
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

type Time = chrono::DateTime<chrono::Local>;

/// A log line split into its timestamp (Vexor's prefix, or a JSON log's
/// own time field) and the rest.
struct Line<'a> {
    ts: Option<Time>,
    text: &'a str,
}

/// `2026-10-23T15:17:05.344+05:30 message` (as the shim writes it).
fn vexor_stamp(line: &str) -> Option<(Time, &str)> {
    let (head, rest) = line.split_once(' ')?;
    if head.len() < 23 || head.as_bytes().get(10) != Some(&b'T') {
        return None;
    }
    let t = chrono::DateTime::parse_from_rfc3339(head).ok()?;
    Some((t.with_timezone(&chrono::Local), rest))
}

/// A JSON log line's own time: pino `time` (epoch ms), winston
/// `timestamp`, `@timestamp`, `ts` (epoch s or ms, or RFC 3339).
fn json_time(v: &serde_json::Value) -> Option<Time> {
    use chrono::TimeZone;
    for key in ["time", "timestamp", "@timestamp", "ts"] {
        match &v[key] {
            serde_json::Value::Number(n) => {
                let x = n.as_f64()?;
                let ms = if x < 1e11 { x * 1000.0 } else { x };
                #[allow(clippy::cast_possible_truncation)]
                return chrono::Local.timestamp_millis_opt(ms as i64).single();
            }
            serde_json::Value::String(s) => {
                if let Ok(t) = chrono::DateTime::parse_from_rfc3339(s) {
                    return Some(t.with_timezone(&chrono::Local));
                }
            }
            _ => {}
        }
    }
    None
}

fn parse(line: &str) -> Line<'_> {
    if let Some((ts, text)) = vexor_stamp(line) {
        return Line { ts: Some(ts), text };
    }
    if line.starts_with('{')
        && let Ok(v) = serde_json::from_str::<serde_json::Value>(line)
    {
        return Line {
            ts: json_time(&v),
            text: line,
        };
    }
    Line {
        ts: None,
        text: line,
    }
}

/// `15:17:05.344`, with the date when it isn't today.
fn clock(ts: Option<Time>) -> String {
    match ts {
        Some(t) if t.date_naive() == chrono::Local::now().date_naive() => {
            t.format("%H:%M:%S%.3f").to_string()
        }
        Some(t) => t.format("%m-%d %H:%M:%S%.3f").to_string(),
        None => " ".repeat(12),
    }
}

/// Pretty JSON logs (pino, winston, bunyan): level, message, other fields.
fn pretty_json(text: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(text).ok()?;
    let obj = v.as_object()?;
    let msg = obj.get("msg").or_else(|| obj.get("message"))?.as_str()?;
    let level = match obj.get("level") {
        Some(serde_json::Value::Number(n)) => match n.as_u64().unwrap_or(30) {
            0..=10 => "trace",
            11..=20 => "debug",
            21..=30 => "info",
            31..=40 => "warn",
            41..=50 => "error",
            _ => "fatal",
        }
        .to_owned(),
        Some(serde_json::Value::String(s)) => s.to_lowercase(),
        _ => "info".to_owned(),
    };
    let badge = match level.as_str() {
        "error" | "fatal" => crate::ui::bold_red(&format!("{level:<5}")),
        "warn" | "warning" => crate::ui::bold_yellow(&format!("{level:<5}")),
        "debug" | "trace" => crate::ui::dim(&format!("{level:<5}")),
        _ => crate::ui::green(&format!("{level:<5}")),
    };
    const SKIP: &[&str] = &[
        "msg",
        "message",
        "level",
        "time",
        "timestamp",
        "@timestamp",
        "ts",
        "pid",
        "hostname",
        "v",
    ];
    let rest: Vec<String> = obj
        .iter()
        .filter(|(k, _)| !SKIP.contains(&k.as_str()))
        .map(|(k, v)| match v {
            serde_json::Value::String(s) => format!("{k}={s}"),
            other => format!("{k}={other}"),
        })
        .collect();
    Some(if rest.is_empty() {
        format!("{badge} {msg}")
    } else {
        format!("{badge} {msg} {}", crate::ui::dim(&rest.join(" ")))
    })
}

/// Print one line: time, app (in its colour), stream marker, message.
fn print_line(
    out: &mut impl Write,
    src: &Source,
    line: &str,
    raw: bool,
    app_width: usize,
    fallback: Option<Time>,
) -> io::Result<()> {
    if raw {
        return writeln!(out, "{line}");
    }
    let l = parse(line);
    let ts = l.ts.or(fallback);
    let name = crate::ui::pad(&crate::ui::app(&src.app), app_width);
    let fancy = crate::ui::fancy();
    let bar = match (src.err && !src.merged, fancy) {
        (true, true) => crate::ui::bold_red("┃"),
        (true, false) => "!".to_owned(),
        (false, true) => crate::ui::dim("│"),
        (false, false) => "|".to_owned(),
    };
    let text = pretty_json(l.text).unwrap_or_else(|| l.text.to_owned());
    writeln!(out, "{} {name} {bar} {text}", crate::ui::dim(&clock(ts)))
}

fn app_width(sources: &[Source]) -> usize {
    crate::ui::assign_app_colors(sources.iter().map(|s| s.app.as_str()));
    sources.iter().map(|s| s.app.len()).max().unwrap_or(0)
}

/// Print the last `n` lines of each source as one timeline: lines from
/// every file are merged by time (a line without a time keeps the time of
/// the line before it, so stack traces stay together).
pub fn print_tail(sources: &[Source], n: usize, raw: bool) -> io::Result<()> {
    let mut out = io::stdout().lock();
    let width = app_width(sources);
    let mut all: Vec<(Option<Time>, usize, usize, String)> = Vec::new();
    for (i, s) in sources.iter().enumerate() {
        let mut last: Option<Time> = None;
        for (seq, line) in last_lines_with_rotated(&s.path, n).into_iter().enumerate() {
            let ts = parse(&line).ts.or(last);
            last = ts;
            all.push((ts, i, seq, line));
        }
    }
    if !raw {
        // Stable: equal times keep file order.
        all.sort_by_key(|(ts, i, seq, _)| (*ts, *i, *seq));
    }
    for (ts, i, _, line) in &all {
        print_line(&mut out, &sources[*i], line, raw, width, *ts)?;
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
                // Read what the old file still had before switching, then
                // any file rotated after it (several rotations can happen
                // between two polls).
                let mut tail = self.read_new();
                for path in self.rotated_after_current() {
                    if let Ok(mut f) = File::open(&path) {
                        let mut buf = Vec::new();
                        let _ = f.read_to_end(&mut buf);
                        self.partial.extend_from_slice(&buf);
                        tail.extend(self.complete_lines());
                    }
                }
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
        self.complete_lines()
    }

    fn complete_lines(&mut self) -> Vec<String> {
        let mut lines = Vec::new();
        while let Some(i) = self.partial.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.partial.drain(..=i).collect();
            lines.push(String::from_utf8_lossy(&line[..line.len() - 1]).into_owned());
        }
        lines
    }

    /// Rotated files newer than the one being read (`path.N` … `path.1`,
    /// oldest first), found by locating its inode in the `.1`, `.2`, …
    /// chain. Empty if it is gone (rotated beyond `retain`).
    fn rotated_after_current(&self) -> Vec<PathBuf> {
        let mut newer = Vec::new();
        for i in 1.. {
            let path = PathBuf::from(format!("{}.{i}", self.source.path.display()));
            let Ok(m) = std::fs::metadata(&path) else {
                return Vec::new();
            };
            if (m.dev(), m.ino()) == self.id {
                newer.reverse();
                return newer;
            }
            newer.push(path);
        }
        Vec::new()
    }
}

/// Print new lines from every source as they arrive, until interrupted.
/// A line without a time of its own is shown with the time it arrived.
pub fn follow(sources: Vec<Source>, raw: bool) -> io::Result<()> {
    let width = app_width(&sources);
    let mut followed: Vec<Followed> = sources.into_iter().map(Followed::new).collect();
    loop {
        let mut out = io::stdout().lock();
        for f in &mut followed {
            for line in f.poll() {
                let now = Some(chrono::Local::now());
                print_line(&mut out, &f.source, &line, raw, width, now)?;
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
    fn times_come_from_the_stamp_or_the_json() {
        let l = parse("2026-10-23T15:17:05.344+05:30 hello world");
        assert_eq!(l.text, "hello world");
        assert!(l.ts.is_some());
        let j = parse(r#"{"level":30,"time":1792742206407,"msg":"up"}"#);
        assert!(j.ts.is_some());
        assert!(parse("plain line").ts.is_none());
        assert!(parse("2026-10-23 nope").ts.is_none());
        let p = pretty_json(r#"{"level":50,"time":1,"msg":"boom","route":"/x","pid":3}"#)
            .expect("pretty");
        assert!(
            p.contains("error")
                && p.contains("boom")
                && p.contains("route=/x")
                && !p.contains("pid")
        );
        assert!(pretty_json(r#"{"no":"msg"}"#).is_none());
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
            app: "a".into(),
            err: false,
            merged: false,
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

    #[test]
    fn follow_reads_every_file_when_rotated_twice_between_polls() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write(dir.path(), "out.log", "");
        let mut f = Followed::new(Source {
            app: "a".into(),
            err: false,
            merged: false,
            path: path.clone(),
        });
        let numbered = |i: u32| dir.path().join(format!("out.log.{i}"));
        let rotate = |next: &str| {
            for i in (1..5).rev() {
                let _ = std::fs::rename(numbered(i), numbered(i + 1));
            }
            std::fs::rename(&path, numbered(1)).expect("rotate");
            std::fs::write(&path, next).expect("new file");
        };
        std::fs::write(&path, "1\n").expect("write");
        rotate("2\n");
        rotate("3\n");
        assert_eq!(f.poll(), ["1", "2", "3"]);
    }
}

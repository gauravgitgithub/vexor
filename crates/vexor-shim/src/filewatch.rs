//! `watch`: restart every instance when files change, as PM2 does.
//!
//! PM2 7 (`lib/Watcher.js`): on the first change it waits `watch_delay`,
//! restarts the app, and ignores changes until then. Vexor waits at least
//! 100 ms, so one save (often several file events) is one restart. By default dot-files
//! and `node_modules` are ignored; `ignore_watch` replaces that list. Unlike
//! PM2, the app's own log files are always ignored (an app logging inside
//! a watched directory would otherwise restart itself forever), and one
//! watcher serves all instances (PM2 has one per instance, so an app with N
//! instances is restarted N times per change).
//!
//! Linux uses inotify; other platforms compare the tree's modification
//! times and sizes every second.

use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use globset::{Glob, GlobSet, GlobSetBuilder};
use tokio::sync::{broadcast, mpsc};
use vexor_core::WatchSpec;

use crate::instance::Control;
use crate::output::{Output, ShimEvent};

/// Which changed paths don't count.
#[derive(Debug, Clone)]
pub struct Matcher {
    globs: GlobSet,
    /// The app's log files (and their rotated copies `<file>.N`).
    logs: Vec<PathBuf>,
}

impl Matcher {
    pub fn new(patterns: &[String], logs: Vec<PathBuf>) -> Result<Self, String> {
        let mut b = GlobSetBuilder::new();
        for p in patterns {
            let glob = Glob::new(p.trim_end_matches('/'))
                .map_err(|e| format!("invalid ignore_watch pattern {p:?}: {e}"))?;
            b.add(glob);
        }
        let globs = b.build().map_err(|e| e.to_string())?;
        Ok(Matcher { globs, logs })
    }

    /// `path` is under `root`. A pattern matches the path relative to the
    /// root, any directory above it, or any single path component; so
    /// `node_modules` ignores everything under any `node_modules`.
    pub fn ignored(&self, root: &Path, path: &Path) -> bool {
        if self.is_log(path) {
            return true;
        }
        let rel = path.strip_prefix(root).unwrap_or(path);
        let mut prefix = PathBuf::new();
        for c in rel.components() {
            let Component::Normal(name) = c else {
                continue;
            };
            prefix.push(name);
            if self.globs.is_match(Path::new(name)) || self.globs.is_match(&prefix) {
                return true;
            }
        }
        false
    }

    fn is_log(&self, path: &Path) -> bool {
        self.logs.iter().any(|log| {
            path == log
                || path
                    .to_str()
                    .zip(log.to_str())
                    .and_then(|(p, l)| p.strip_prefix(l)?.strip_prefix('.'))
                    .is_some_and(|n| n.bytes().all(|b| b.is_ascii_digit()))
        })
    }
}

pub fn matcher(spec: &WatchSpec, logs: Vec<PathBuf>) -> Result<Matcher, String> {
    Matcher::new(&spec.ignore, logs)
}

/// Restart every instance when a watched file changes. Runs until the
/// shim stops.
pub async fn run(
    spec: WatchSpec,
    matcher: Matcher,
    app: String,
    out: Output,
    restart: broadcast::Sender<Control>,
) {
    let (tx, mut rx) = mpsc::unbounded_channel::<PathBuf>();
    let roots = spec.paths.clone();
    tokio::spawn(backend::watch(roots, matcher, tx));
    // One save is often several events (create, write, rename): changes
    // within 100 ms of the first make one restart.
    let delay = Duration::from_millis(spec.delay_ms.max(100));
    while let Some(path) = rx.recv().await {
        out.emit(&ShimEvent::WatchRestart {
            app: &app,
            path: &path,
        });
        tokio::time::sleep(delay).await;
        // Changes until now are covered by this restart.
        while rx.try_recv().is_ok() {}
        let _ = restart.send(Control::Restart);
    }
}

#[cfg(target_os = "linux")]
mod backend {
    //! inotify: one watch per directory, added as directories appear.

    use std::collections::HashMap;
    use std::mem::MaybeUninit;
    use std::os::fd::OwnedFd;
    use std::path::{Path, PathBuf};

    use std::os::unix::ffi::OsStrExt;

    use rustix::fs::inotify::{self, CreateFlags, ReadFlags, WatchFlags};
    use tokio::io::unix::AsyncFd;
    use tokio::sync::mpsc;

    use super::Matcher;

    const MASK: WatchFlags = WatchFlags::CREATE
        .union(WatchFlags::DELETE)
        .union(WatchFlags::MODIFY)
        .union(WatchFlags::MOVED_FROM)
        .union(WatchFlags::MOVED_TO)
        .union(WatchFlags::DELETE_SELF)
        .union(WatchFlags::DONT_FOLLOW);

    struct Watches {
        fd: AsyncFd<OwnedFd>,
        /// Watch descriptor -> (its root, the watched path).
        dirs: HashMap<i32, (PathBuf, PathBuf)>,
        matcher: Matcher,
    }

    impl Watches {
        /// Watch `path` and, if it is a directory, everything below it that
        /// isn't ignored. Symbolic links are not followed.
        fn add_tree(&mut self, root: &Path, path: &Path) {
            if path != root && self.matcher.ignored(root, path) {
                return;
            }
            let Ok(meta) = std::fs::symlink_metadata(path) else {
                return;
            };
            if !(meta.is_dir() || path == root) {
                return;
            }
            match inotify::add_watch(self.fd.get_ref(), path, MASK) {
                Ok(wd) => {
                    self.dirs
                        .insert(wd, (root.to_path_buf(), path.to_path_buf()));
                }
                Err(e) => {
                    eprintln!("vexor shim: cannot watch {}: {e}", path.display());
                    return;
                }
            }
            if meta.is_dir()
                && let Ok(entries) = std::fs::read_dir(path)
            {
                for e in entries.flatten() {
                    if e.file_type().is_ok_and(|t| t.is_dir()) {
                        self.add_tree(root, &e.path());
                    }
                }
            }
        }
    }

    #[allow(unsafe_code)]
    fn register(fd: OwnedFd) -> std::io::Result<AsyncFd<OwnedFd>> {
        // SAFETY: the `OwnedFd` is a valid, open descriptor, owned by the
        // `AsyncFd` for its whole lifetime, and `as_raw_fd` on it always
        // returns that same descriptor.
        unsafe { AsyncFd::register(fd) }.map_err(|e| e.into_parts().1)
    }

    pub async fn watch(roots: Vec<PathBuf>, matcher: Matcher, tx: mpsc::UnboundedSender<PathBuf>) {
        let fd = match inotify::init(CreateFlags::NONBLOCK | CreateFlags::CLOEXEC)
            .map_err(std::io::Error::from)
            .and_then(register)
        {
            Ok(fd) => fd,
            Err(e) => {
                eprintln!("vexor shim: cannot watch files: {e}");
                return;
            }
        };
        let mut w = Watches {
            fd,
            dirs: HashMap::new(),
            matcher,
        };
        for r in &roots {
            w.add_tree(r, r);
        }
        let mut buf = [MaybeUninit::<u8>::uninit(); 8192];
        loop {
            let Ok(mut guard) = w.fd.readable().await else {
                return;
            };
            let mut changed: Vec<PathBuf> = Vec::new();
            let mut new_dirs: Vec<(PathBuf, PathBuf)> = Vec::new();
            let mut gone: Vec<i32> = Vec::new();
            let result = guard.try_io(|fd| -> std::io::Result<()> {
                let mut reader = inotify::Reader::new(fd.get_ref(), &mut buf);
                loop {
                    match reader.next() {
                        Ok(ev) => {
                            let flags = ev.events();
                            if flags.contains(ReadFlags::QUEUE_OVERFLOW) {
                                // Events were lost: treat it as a change.
                                if let Some(r) = roots.first() {
                                    changed.push(r.clone());
                                }
                                continue;
                            }
                            if flags.contains(ReadFlags::IGNORED) {
                                gone.push(ev.wd());
                                continue;
                            }
                            let Some((root, dir)) = w.dirs.get(&ev.wd()) else {
                                continue;
                            };
                            let path = match ev.file_name() {
                                Some(n) => dir.join(std::ffi::OsStr::from_bytes(n.to_bytes())),
                                None => dir.clone(),
                            };
                            if flags.contains(ReadFlags::ISDIR)
                                && flags.intersects(ReadFlags::CREATE | ReadFlags::MOVED_TO)
                            {
                                new_dirs.push((root.clone(), path.clone()));
                            }
                            if !w.matcher.ignored(root, &path) {
                                changed.push(path);
                            }
                        }
                        Err(e) if e == rustix::io::Errno::WOULDBLOCK => {
                            return Err(std::io::ErrorKind::WouldBlock.into());
                        }
                        Err(e) => return Err(e.into()),
                    }
                }
            });
            if let Ok(Err(e)) = result {
                eprintln!("vexor shim: file watch failed: {e}");
                return;
            }
            for wd in gone {
                w.dirs.remove(&wd);
            }
            for (root, dir) in new_dirs {
                w.add_tree(&root, &dir);
            }
            for p in changed {
                if tx.send(p).is_err() {
                    return;
                }
            }
        }
    }
}

#[cfg(not(target_os = "linux"))]
mod backend {
    //! Polling: compare modification times and sizes every second.

    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::time::{Duration, SystemTime};

    use tokio::sync::mpsc;

    use super::Matcher;

    const INTERVAL: Duration = Duration::from_secs(1);
    /// Stop descending after this many entries, so a huge tree can't make
    /// each pass expensive; reported once.
    const MAX_ENTRIES: usize = 100_000;

    type Snapshot = HashMap<PathBuf, (Option<SystemTime>, u64)>;

    fn scan(roots: &[PathBuf], matcher: &Matcher) -> (Snapshot, bool) {
        let mut snap = Snapshot::new();
        let mut truncated = false;
        for root in roots {
            let mut stack = vec![root.clone()];
            while let Some(path) = stack.pop() {
                if snap.len() >= MAX_ENTRIES {
                    truncated = true;
                    break;
                }
                if path != *root && matcher.ignored(root, &path) {
                    continue;
                }
                let Ok(meta) = std::fs::symlink_metadata(&path) else {
                    continue;
                };
                // A directory's own time changes whenever an entry is added or
                // removed, which the entry itself shows (and may be ignored):
                // only its presence counts.
                let state = if meta.is_dir() {
                    (None, 0)
                } else {
                    (meta.modified().ok(), meta.len())
                };
                snap.insert(path.clone(), state);
                if meta.is_dir()
                    && let Ok(entries) = std::fs::read_dir(&path)
                {
                    stack.extend(entries.flatten().map(|e| e.path()));
                }
            }
        }
        (snap, truncated)
    }

    fn diff(old: &Snapshot, new: &Snapshot) -> Option<PathBuf> {
        new.iter()
            .find(|(p, v)| old.get(*p) != Some(v))
            .map(|(p, _)| p.clone())
            .or_else(|| old.keys().find(|p| !new.contains_key(*p)).cloned())
    }

    pub async fn watch(roots: Vec<PathBuf>, matcher: Matcher, tx: mpsc::UnboundedSender<PathBuf>) {
        let scan_now = {
            let roots = roots.clone();
            let matcher = matcher.clone();
            move || scan(&roots, &matcher)
        };
        let Ok((mut prev, truncated)) = tokio::task::spawn_blocking(scan_now.clone()).await else {
            return;
        };
        if truncated {
            eprintln!(
                "vexor shim: watching only the first {MAX_ENTRIES} files; add ignore_watch patterns"
            );
        }
        loop {
            tokio::time::sleep(INTERVAL).await;
            let Ok((cur, _)) = tokio::task::spawn_blocking(scan_now.clone()).await else {
                return;
            };
            if let Some(p) = diff(&prev, &cur)
                && tx.send(p).is_err()
            {
                return;
            }
            prev = cur;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(patterns: &[&str]) -> Matcher {
        Matcher::new(
            &patterns.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>(),
            vec![PathBuf::from("/app/logs/out.log")],
        )
        .expect("matcher")
    }

    #[test]
    fn pm2_default_ignores_dot_paths_and_node_modules() {
        let d = m(&[".*", "node_modules"]);
        let root = Path::new("/app");
        assert!(d.ignored(root, Path::new("/app/.git/HEAD")));
        assert!(d.ignored(root, Path::new("/app/src/.swp")));
        assert!(d.ignored(root, Path::new("/app/node_modules/x/index.js")));
        assert!(d.ignored(root, Path::new("/app/lib/node_modules/y.js")));
        assert!(!d.ignored(root, Path::new("/app/src/index.js")));
    }

    #[test]
    fn globs_match_paths_components_and_directories() {
        let d = m(&["*.log", "tmp/**", "dist"]);
        let root = Path::new("/app");
        assert!(d.ignored(root, Path::new("/app/a/b.log")));
        assert!(d.ignored(root, Path::new("/app/tmp/x/y.js")));
        assert!(d.ignored(root, Path::new("/app/dist/bundle.js")));
        assert!(!d.ignored(root, Path::new("/app/src/dist.js")));
    }

    #[test]
    fn own_logs_are_always_ignored() {
        let d = m(&[]);
        let root = Path::new("/app");
        assert!(d.ignored(root, Path::new("/app/logs/out.log")));
        assert!(d.ignored(root, Path::new("/app/logs/out.log.3")));
        assert!(!d.ignored(root, Path::new("/app/logs/out.log.bak")));
        assert!(!d.ignored(root, Path::new("/app/logs/other.log")));
    }

    #[test]
    fn invalid_patterns_are_errors() {
        assert!(Matcher::new(&["a[".to_owned()], Vec::new()).is_err());
    }
}

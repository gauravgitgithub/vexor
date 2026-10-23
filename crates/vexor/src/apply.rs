//! `vexor migrate --from-pm2 --apply`: move apps from PM2 to Vexor, one at a
//! time, journaled (docs/migration.md).
//!
//! Each app: `pm2 stop` (never `delete`, so PM2 keeps its definition), start
//! under Vexor, wait until healthy. An app Vexor can't run healthily is
//! deleted from Vexor and started again under PM2, and the run stops there.
//! Every step is appended to `journal.jsonl` in the backup directory before
//! moving on, so an interrupted run resumes where it stopped.

use std::collections::BTreeMap;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use vexor_core::AppSpec;
use vexor_proto::{AppStatus, Request, Response, Target};

use crate::migrate::{self, Downtime, Manifest, Report};

pub struct Options {
    pub backup_id: Option<String>,
    pub only: Vec<String>,
    pub yes: bool,
    pub pm2: Option<PathBuf>,
    /// Longest wait for an app to become healthy under Vexor.
    pub timeout: Duration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Step {
    /// About to move the app; Vexor had no app by this name.
    Begin,
    Pm2Stopped,
    VexorStarted,
    /// Running (or added stopped) under Vexor.
    Done,
    /// Vexor couldn't run it; deleted from Vexor and back under PM2.
    RolledBack,
    /// Moving it failed and putting it back failed too: needs attention.
    Failed,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Entry {
    pub ts: String,
    pub app: String,
    pub step: Step,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

pub const JOURNAL: &str = "journal.jsonl";

/// Read a backup's journal. Missing file = empty journal.
pub fn read_journal(dir: &Path) -> Result<Vec<Entry>, String> {
    let path = dir.join(JOURNAL);
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
    };
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    let mut out = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        match serde_json::from_str(line) {
            Ok(e) => out.push(e),
            // A torn last line from a crash mid-write is ignored.
            Err(_) if i + 1 == lines.len() => {}
            Err(e) => return Err(format!("{} line {}: {e}", path.display(), i + 1)),
        }
    }
    Ok(out)
}

/// Last step recorded for each app.
pub fn last_steps(journal: &[Entry]) -> BTreeMap<String, Step> {
    journal.iter().map(|e| (e.app.clone(), e.step)).collect()
}

struct Journal {
    file: std::fs::File,
}

impl Journal {
    fn open(dir: &Path) -> Result<Self, String> {
        use std::os::unix::fs::OpenOptionsExt;
        let path = dir.join(JOURNAL);
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(&path)
            .map_err(|e| format!("cannot open {}: {e}", path.display()))?;
        Ok(Journal { file })
    }

    /// Append one step and flush it to disk before going on.
    fn write(&mut self, app: &str, step: Step, detail: Option<String>) -> Result<(), String> {
        let e = Entry {
            ts: format!("{} UTC", crate::format_utc(crate::describe::now_ms())),
            app: app.to_owned(),
            step,
            detail,
        };
        let mut line = serde_json::to_string(&e).map_err(|e| e.to_string())?;
        line.push('\n');
        self.file
            .write_all(line.as_bytes())
            .and_then(|()| self.file.sync_data())
            .map_err(|e| format!("cannot write the migration journal: {e}"))
    }
}

/// A backup directory with its manifest.
pub struct Backup {
    pub dir: PathBuf,
    pub manifest: Manifest,
}

/// The named backup, or the newest one.
pub fn find_backup(home: &Path, id: Option<&str>) -> Result<Backup, String> {
    let root = migrate::migrations_dir(home);
    let id = match id {
        Some(id) => id.to_owned(),
        None => {
            let mut ids: Vec<String> = std::fs::read_dir(&root)
                .map(|d| {
                    d.filter_map(Result::ok)
                        .map(|e| e.file_name().to_string_lossy().into_owned())
                        .filter(|n| !n.starts_with('.'))
                        .collect()
                })
                .unwrap_or_default();
            // Ids are UTC times (with -2, -3 … within a second): sort by
            // time, then by sequence number.
            ids.sort_by_key(|i| {
                let (t, n) = match i.get(15..) {
                    Some(rest) if rest.starts_with('-') => {
                        (&i[..15], rest[1..].parse().unwrap_or(0))
                    }
                    _ => (i.as_str(), 1),
                };
                (t.to_owned(), n)
            });
            ids.pop().ok_or_else(|| {
                "no migration backup found; run `vexor migrate --from-pm2 --backup` first and review its plan"
                    .to_owned()
            })?
        }
    };
    if id.contains('/') || id.starts_with('.') {
        return Err(format!("invalid backup id {id:?}"));
    }
    let dir = root.join(&id);
    let (m, _) = migrate::read_regular(&dir.join("manifest.json"))
        .map_err(|e| format!("backup {id}: cannot read manifest.json: {e}"))?;
    let manifest: Manifest = serde_json::from_slice(&m)
        .map_err(|e| format!("backup {id}: invalid manifest.json: {e}"))?;
    Ok(Backup { dir, manifest })
}

fn find_pm2(explicit: Option<PathBuf>) -> Result<PathBuf, String> {
    if let Some(p) = explicit {
        return Ok(p);
    }
    let path = std::env::var("PATH").unwrap_or_default();
    std::env::split_paths(&path)
        .map(|d| d.join("pm2"))
        .find(|p| p.is_file())
        .ok_or_else(|| "`pm2` is not on PATH; pass --pm2 /path/to/pm2".to_owned())
}

struct Pm2 {
    bin: PathBuf,
    home: PathBuf,
}

impl Pm2 {
    /// Wait until every PM2 process named `name` is online.
    fn wait_online(&self, name: &str, timeout: Duration) -> Result<(), String> {
        let deadline = Instant::now() + timeout;
        let mut last = String::from("no status");
        while Instant::now() < deadline {
            let out = Command::new(&self.bin)
                .arg("jlist")
                .env("PM2_HOME", &self.home)
                .output()
                .map_err(|e| format!("cannot run {}: {e}", self.bin.display()))?;
            let text = String::from_utf8_lossy(&out.stdout);
            // pm2 may print notices before the JSON.
            let json = text.find('[').map_or("[]", |i| &text[i..]);
            if let Ok(serde_json::Value::Array(list)) = serde_json::from_str(json) {
                let st: Vec<&str> = list
                    .iter()
                    .filter(|p| p["name"] == name)
                    .filter_map(|p| p["pm2_env"]["status"].as_str())
                    .collect();
                if !st.is_empty() && st.iter().all(|s| *s == "online") {
                    return Ok(());
                }
                last = format!("{st:?}");
            }
            std::thread::sleep(Duration::from_millis(300));
        }
        Err(format!(
            "not online under PM2 within {}s: {last}",
            timeout.as_secs()
        ))
    }

    fn run(&self, args: &[&str]) -> Result<(), String> {
        let out = Command::new(&self.bin)
            .args(args)
            .env("PM2_HOME", &self.home)
            .output()
            .map_err(|e| format!("cannot run {}: {e}", self.bin.display()))?;
        if out.status.success() {
            Ok(())
        } else {
            let err = String::from_utf8_lossy(&out.stderr);
            let msg = err
                .lines()
                .rev()
                .find(|l| !l.trim().is_empty())
                .unwrap_or("");
            Err(format!(
                "`pm2 {}` failed ({}): {}",
                args.join(" "),
                out.status,
                msg.trim()
            ))
        }
    }
}

fn vexor_apps(home: &Path) -> Result<Vec<AppStatus>, String> {
    match crate::client::request(home, &Request::List)? {
        Response::Ok { apps, .. } => Ok(apps),
        Response::Error { message, .. } => Err(message),
        _ => Err("unexpected response from the daemon".into()),
    }
}

fn vexor_ok(home: &Path, req: &Request) -> Result<(), String> {
    match crate::client::request(home, req)? {
        Response::Error { message, .. } => Err(message),
        _ => Ok(()),
    }
}

/// Healthy: every instance online with no restart, and the same live
/// processes, for `settle`. The process check guards against a status that
/// lags behind a process that already exited.
fn wait_healthy(home: &Path, spec: &AppSpec, timeout: Duration) -> Result<(), String> {
    let settle = Duration::from_millis(spec.policy.min_uptime_ms.clamp(1000, 15_000));
    let deadline =
        Instant::now() + timeout + Duration::from_millis(spec.policy.ready_timeout_ms.unwrap_or(0));
    let mut healthy: Option<(Instant, Vec<u64>)> = None;
    let mut last = String::from("no status yet");
    while Instant::now() < deadline {
        let apps = vexor_apps(home)?;
        let snap = apps
            .iter()
            .find(|a| a.name == spec.name)
            .map(|a| a.snapshot.clone())
            .unwrap_or_default();
        let states: Vec<&str> = snap.iter().filter_map(|s| s["state"].as_str()).collect();
        let restarts: u64 = snap.iter().filter_map(|s| s["restarts"].as_u64()).sum();
        let mut pids: Vec<u64> = snap.iter().filter_map(|s| s["pid"].as_u64()).collect();
        pids.sort_unstable();
        last = format!("states {states:?}, {restarts} restarts");
        if restarts > 0 || states.iter().any(|s| matches!(*s, "errored" | "backoff")) {
            return Err(format!("not healthy under Vexor: {last}"));
        }
        let ok = states.len() == spec.instances as usize
            && pids.len() == states.len()
            && states.iter().all(|s| *s == "online")
            && pids.iter().all(|p| alive(*p));
        healthy = match healthy {
            Some((t, p)) if ok && p == pids => {
                if t.elapsed() >= settle {
                    return Ok(());
                }
                Some((t, p))
            }
            _ if ok => Some((Instant::now(), pids)),
            _ => None,
        };
        std::thread::sleep(Duration::from_millis(200));
    }
    Err(format!("not healthy within {}s: {last}", timeout.as_secs()))
}

fn alive(pid: u64) -> bool {
    i32::try_from(pid)
        .ok()
        .and_then(rustix::process::Pid::from_raw)
        .is_some_and(|p| rustix::process::test_kill_process(p).is_ok())
}

fn confirm(question: &str) -> bool {
    print!("{question} [y/N] ");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    std::io::stdin().read_line(&mut line).is_ok() && matches!(line.trim(), "y" | "Y" | "yes")
}

/// Returns the process exit code.
pub fn run(home: &Path, pm2_home: &Path, dump: &Path, opts: Options) -> u8 {
    match apply(home, pm2_home, dump, opts) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("vexor: {e}");
            1
        }
    }
}

#[allow(clippy::too_many_lines)]
fn apply(home: &Path, pm2_home: &Path, dump: &Path, opts: Options) -> Result<u8, String> {
    let backup = find_backup(home, opts.backup_id.as_deref())?;
    let id = backup.manifest.id.clone();

    // Apply exactly what was reviewed: the dump must be unchanged since the
    // backup, and the backup's plan intact.
    let (now, _) =
        migrate::read_regular(dump).map_err(|e| format!("cannot read {}: {e}", dump.display()))?;
    let saved = backup
        .manifest
        .files
        .iter()
        .find(|f| f.copy == "pm2/dump.pm2")
        .ok_or_else(|| format!("backup {id} has no dump.pm2"))?;
    if migrate::sha256(&now) != saved.sha256 {
        return Err(format!(
            "{} changed since backup {id}; run `vexor migrate --from-pm2 --backup` again and review the new plan",
            dump.display()
        ));
    }
    let (plan_bytes, _) = migrate::read_regular(&backup.dir.join("plan.json"))
        .map_err(|e| format!("backup {id}: cannot read plan.json: {e}"))?;
    if migrate::sha256(&plan_bytes) != backup.manifest.plan_sha256 {
        return Err(format!(
            "backup {id}: plan.json does not match its manifest"
        ));
    }

    let report: Report = migrate::plan(dump, pm2_home)?;
    for o in &opts.only {
        if !report.apps.iter().any(|a| &a.name == o) {
            return Err(format!("--only {o}: no app with that name in the PM2 dump"));
        }
    }
    let selected: Vec<&migrate::AppPlan> = report
        .apps
        .iter()
        .filter(|a| opts.only.is_empty() || opts.only.contains(&a.name))
        .collect();

    // One migration at a time per backup.
    let _lock = vexor_shim::try_lock(&backup.dir.join(".lock"))
        .map_err(|e| format!("cannot lock backup {id}: {e}"))?
        .ok_or_else(|| format!("another `vexor migrate --apply` is using backup {id}"))?;
    let journal_entries = read_journal(&backup.dir)?;
    let last = last_steps(&journal_entries);

    let todo: Vec<&migrate::AppPlan> = selected
        .iter()
        .copied()
        .filter(|a| a.downtime != Downtime::Blocked && last.get(&a.name) != Some(&Step::Done))
        .collect();
    let blocked: Vec<&str> = selected
        .iter()
        .filter(|a| a.downtime == Downtime::Blocked)
        .map(|a| a.name.as_str())
        .collect();
    let already: Vec<&str> = selected
        .iter()
        .filter(|a| last.get(&a.name) == Some(&Step::Done))
        .map(|a| a.name.as_str())
        .collect();

    println!("Migration {id} from {}", dump.display());
    if !already.is_empty() {
        println!("  already migrated: {}", already.join(", "));
    }
    if !blocked.is_empty() {
        println!(
            "  staying on PM2 (blocked, see `vexor migrate --from-pm2`): {}",
            blocked.join(", ")
        );
    }
    if todo.is_empty() {
        println!("Nothing to migrate.");
        return Ok(if blocked.is_empty() {
            0
        } else {
            crate::code::PARTIAL
        });
    }
    for a in &todo {
        let what = match a.downtime {
            Downtime::Brief => "stop under PM2, start under Vexor",
            _ => "add to Vexor stopped (not running under PM2)",
        };
        println!("  {}: {what}", a.name);
    }
    println!(
        "An app Vexor can't run healthily within {}s is deleted from Vexor and started again under PM2, and the migration stops there.",
        opts.timeout.as_secs()
    );
    if !opts.yes {
        if !std::io::stdin().is_terminal() {
            return Err("not a terminal: pass --yes to confirm the migration".into());
        }
        if !confirm("Migrate these apps?") {
            println!("Nothing was changed.");
            return Ok(1);
        }
    }

    let pm2 = if todo.iter().any(|a| a.running) {
        Some(Pm2 {
            bin: find_pm2(opts.pm2)?,
            home: pm2_home.to_path_buf(),
        })
    } else {
        None
    };
    let mut journal = Journal::open(&backup.dir)?;
    let existing: Vec<String> = vexor_apps(home)?.into_iter().map(|a| a.name).collect();
    let mut left_on_pm2: Vec<String> = Vec::new();

    for a in todo {
        let name = a.name.as_str();
        let spec = a
            .full_spec
            .clone()
            .ok_or_else(|| format!("{name}: no spec (blocked)"))?;
        let resumed = matches!(
            last.get(name),
            Some(Step::Begin | Step::Pm2Stopped | Step::VexorStarted)
        );
        if !resumed && existing.iter().any(|e| e == name) {
            journal.write(
                name,
                Step::Failed,
                Some("Vexor already has an app by this name".into()),
            )?;
            eprintln!(
                "vexor: {name}: Vexor already has an app by this name; PM2 was not touched. \
                 Delete or rename it in Vexor, then run --apply again."
            );
            return Ok(1);
        }
        print!("{name}: ");
        let _ = std::io::stdout().flush();
        if !resumed {
            journal.write(name, Step::Begin, None)?;
        }
        if a.running {
            let pm2 = pm2.as_ref().expect("pm2 found for running apps");
            if let Err(e) = pm2.run(&["stop", name]) {
                // Nothing changed for this app yet.
                journal.write(name, Step::Failed, Some(e.clone()))?;
                println!("failed");
                eprintln!("vexor: {name}: {e}; the app was not moved");
                return Ok(1);
            }
            journal.write(name, Step::Pm2Stopped, None)?;
            print!("stopped under PM2, ");
            let _ = std::io::stdout().flush();
        }
        let in_vexor = resumed && vexor_apps(home)?.iter().any(|x| x.name == name);
        let started = if in_vexor {
            vexor_ok(
                home,
                &Request::StartExisting {
                    target: Target::Name(name.to_owned()),
                },
            )
        } else {
            vexor_ok(
                home,
                &Request::Start {
                    spec: Box::new(spec.clone()),
                    start: a.running,
                },
            )
        };
        let health = started.and_then(|()| {
            journal.write(name, Step::VexorStarted, None)?;
            if a.running {
                wait_healthy(home, &spec, opts.timeout)
            } else {
                Ok(())
            }
        });
        match health {
            Ok(()) => {
                journal.write(name, Step::Done, None)?;
                println!(
                    "{}",
                    if a.running {
                        "running under Vexor"
                    } else {
                        "added to Vexor (stopped)"
                    }
                );
            }
            Err(why) => {
                println!("not healthy");
                eprintln!("vexor: {name}: {why}");
                let back = vexor_ok(
                    home,
                    &Request::Delete {
                        target: Target::Name(name.to_owned()),
                    },
                )
                .and_then(|()| match (&pm2, a.running) {
                    (Some(p), true) => p.run(&["start", name]),
                    _ => Ok(()),
                });
                // An app that was already failing under PM2 says nothing
                // about the move: leave it there and carry on.
                let failing_before = a
                    .pm2_status
                    .iter()
                    .any(|s| matches!(s.as_str(), "waiting restart" | "errored"));
                match back {
                    Ok(()) if failing_before => {
                        journal.write(
                            name,
                            Step::RolledBack,
                            Some(format!("already failing under PM2; {why}")),
                        )?;
                        eprintln!(
                            "vexor: {name}: it was already failing under PM2 (it keeps exiting); left on PM2, \
                             continuing with the other apps. Its error log says why."
                        );
                        left_on_pm2.push(name.to_owned());
                        continue;
                    }
                    Ok(()) => {
                        journal.write(name, Step::RolledBack, Some(why))?;
                        eprintln!(
                            "vexor: {name}: removed from Vexor{}; the migration stopped here. \
                             Fix the cause, then run --apply again to continue.",
                            if a.running {
                                " and running under PM2 again"
                            } else {
                                ""
                            }
                        );
                    }
                    Err(e) => {
                        journal.write(name, Step::Failed, Some(format!("{why}; rollback: {e}")))?;
                        eprintln!(
                            "vexor: {name}: putting it back failed: {e}. Check `pm2 ls` and `vexor ls`."
                        );
                    }
                }
                return Ok(1);
            }
        }
    }

    if left_on_pm2.is_empty() {
        println!("\nMigration {id} complete.");
    } else {
        println!(
            "\nMigration {id} finished. Left on PM2 because they were already failing there: {}. \
             Fix them, then run --apply again to move them.",
            left_on_pm2.join(", ")
        );
    }
    println!(
        "Next: run `pm2 unstartup` so PM2 no longer starts these apps at boot (its saved list \
         still has them as running), then `vexor startup`. PM2 keeps them stopped; \
         `pm2 delete` them once you are satisfied."
    );
    Ok(if blocked.is_empty() && left_on_pm2.is_empty() {
        0
    } else {
        crate::code::PARTIAL
    })
}

// ---------------------------------------------------------------- rollback

/// What a migration did to one app, from the journal entries since its last
/// `begin`.
#[derive(Debug, PartialEq, Eq)]
struct Moved {
    /// `pm2 stop` ran, so PM2 has to start it again.
    pm2_stopped: bool,
}

/// Apps to move back, most recently moved first. Apps never begun (e.g.
/// refused because Vexor had the name) or already rolled back are skipped.
fn to_roll_back(journal: &[Entry]) -> Vec<(String, Moved)> {
    let mut order: Vec<String> = Vec::new();
    let mut since: BTreeMap<String, Vec<Step>> = BTreeMap::new();
    for e in journal {
        if e.step == Step::Begin {
            order.retain(|a| a != &e.app);
            order.push(e.app.clone());
            since.insert(e.app.clone(), Vec::new());
        }
        if let Some(steps) = since.get_mut(&e.app) {
            steps.push(e.step);
        }
    }
    order
        .into_iter()
        .rev()
        .filter_map(|app| {
            let steps = &since[&app];
            if steps.last() == Some(&Step::RolledBack) {
                return None;
            }
            let moved = Moved {
                pm2_stopped: steps.contains(&Step::Pm2Stopped),
            };
            Some((app, moved))
        })
        .collect()
}

pub struct RollbackOptions {
    pub backup_id: Option<String>,
    pub only: Vec<String>,
    pub yes: bool,
    pub pm2: Option<PathBuf>,
    pub timeout: Duration,
}

pub fn run_rollback(home: &Path, opts: RollbackOptions) -> u8 {
    match rollback(home, opts) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("vexor: {e}");
            1
        }
    }
}

fn rollback(home: &Path, opts: RollbackOptions) -> Result<u8, String> {
    let backup = find_backup(home, opts.backup_id.as_deref())?;
    let id = backup.manifest.id.clone();
    let _lock = vexor_shim::try_lock(&backup.dir.join(".lock"))
        .map_err(|e| format!("cannot lock backup {id}: {e}"))?
        .ok_or_else(|| format!("another `vexor migrate` is using backup {id}"))?;
    let journal_entries = read_journal(&backup.dir)?;
    let mut apps = to_roll_back(&journal_entries);
    for o in &opts.only {
        if !apps.iter().any(|(a, _)| a == o) {
            return Err(format!(
                "--only {o}: migration {id} has nothing to undo for that app"
            ));
        }
    }
    if !opts.only.is_empty() {
        apps.retain(|(a, _)| opts.only.contains(a));
    }
    if apps.is_empty() {
        println!("Migration {id}: nothing to roll back.");
        return Ok(0);
    }
    let pm2 = if apps.iter().any(|(_, m)| m.pm2_stopped) {
        Some(Pm2 {
            bin: find_pm2(opts.pm2)?,
            home: backup.manifest.pm2_home.clone(),
        })
    } else {
        None
    };
    if !opts.yes && !std::io::stdin().is_terminal() {
        return Err("not a terminal: pass --yes to confirm the rollback".into());
    }
    let mut journal = Journal::open(&backup.dir)?;
    println!("Rolling back migration {id}");
    for (name, moved) in apps {
        let what = if moved.pm2_stopped {
            "delete from Vexor, start under PM2 again"
        } else {
            "delete from Vexor (PM2 wasn't running it)"
        };
        if !opts.yes && !confirm(&format!("{name}: {what}?")) {
            println!("Stopped; {name} and the apps after it were left as they are.");
            return Ok(1);
        }
        let mut done = Vec::new();
        // Any Vexor app by this name is the migrated one: `begin` is only
        // written after checking Vexor had no app by that name.
        if vexor_apps(home)?.iter().any(|a| a.name == name) {
            vexor_ok(
                home,
                &Request::Delete {
                    target: Target::Name(name.clone()),
                },
            )
            .map_err(|e| format!("{name}: cannot delete from Vexor: {e}"))?;
            done.push("deleted from Vexor");
        }
        if moved.pm2_stopped {
            let p = pm2.as_ref().expect("pm2 found");
            if let Err(e) = p
                .run(&["start", &name])
                .and_then(|()| p.wait_online(&name, opts.timeout))
            {
                println!(
                    "{name}: {}failed to start under PM2",
                    done.iter().map(|d| format!("{d}, ")).collect::<String>()
                );
                journal.write(&name, Step::Failed, Some(format!("rollback: {e}")))?;
                eprintln!(
                    "vexor: {name}: {e}. It is not running under Vexor either: check `pm2 ls`."
                );
                return Ok(1);
            }
            done.push("online under PM2");
        }
        if done.is_empty() {
            done.push("nothing to undo");
        }
        println!("{name}: {}", done.join(", "));
        journal.write(&name, Step::RolledBack, Some("rollback".into()))?;
    }
    println!(
        "Rolled back. If you ran `pm2 unstartup`, run `pm2 startup` again; `vexor unstartup` \
         if Vexor no longer runs anything."
    );
    Ok(0)
}

// ------------------------------------------------------------------ status

pub fn status(home: &Path, id: Option<&str>, json: bool) -> u8 {
    let root = migrate::migrations_dir(home);
    let ids: Vec<String> = match id {
        Some(i) => vec![i.to_owned()],
        None => {
            let mut v: Vec<String> = std::fs::read_dir(&root)
                .map(|d| {
                    d.filter_map(Result::ok)
                        .map(|e| e.file_name().to_string_lossy().into_owned())
                        .filter(|n| !n.starts_with('.'))
                        .collect()
                })
                .unwrap_or_default();
            v.sort();
            v
        }
    };
    let mut out = Vec::new();
    for i in &ids {
        let backup = match find_backup(home, Some(i)) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("vexor: {e}");
                return 1;
            }
        };
        let journal = match read_journal(&backup.dir) {
            Ok(j) => j,
            Err(e) => {
                eprintln!("vexor: {e}");
                return 1;
            }
        };
        let apps: BTreeMap<String, &Entry> = journal.iter().map(|e| (e.app.clone(), e)).collect();
        out.push(serde_json::json!({
            "id": backup.manifest.id,
            "created_at": backup.manifest.created_at,
            "dir": backup.dir,
            "pm2_home": backup.manifest.pm2_home,
            "apps": apps.iter().map(|(a, e)| serde_json::json!({
                "app": a, "step": e.step, "at": e.ts, "detail": e.detail,
            })).collect::<Vec<_>>(),
        }));
    }
    if json {
        println!("{}", serde_json::to_string_pretty(&out).unwrap_or_default());
        return 0;
    }
    if out.is_empty() {
        println!("No migrations yet (`vexor migrate --from-pm2 --backup` starts one).");
        return 0;
    }
    for m in &out {
        let apps = m["apps"].as_array().map_or(&[][..], Vec::as_slice);
        println!(
            "{}  created {}  {}",
            m["id"].as_str().unwrap_or("?"),
            m["created_at"].as_str().unwrap_or("?"),
            if apps.is_empty() {
                "backup only (not applied)"
            } else {
                ""
            }
        );
        for a in apps {
            let step = a["step"].as_str().unwrap_or("?");
            let label = match step {
                "done" => "migrated",
                "rolled_back" => "back on PM2",
                "failed" => "FAILED",
                _ => "interrupted (run --apply to continue, or --rollback)",
            };
            println!(
                "  {:<20} {label:<12} {}{}",
                a["app"].as_str().unwrap_or("?"),
                a["at"].as_str().unwrap_or(""),
                a["detail"]
                    .as_str()
                    .map(|d| format!("  ({d})"))
                    .unwrap_or_default()
            );
        }
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(app: &str, step: Step) -> Entry {
        Entry {
            ts: String::new(),
            app: app.into(),
            step,
            detail: None,
        }
    }

    #[test]
    fn rollback_order_and_scope() {
        use Step::*;
        let j = [
            e("a", Begin),
            e("a", Pm2Stopped),
            e("a", VexorStarted),
            e("a", Done),
            e("b", Begin),
            e("b", VexorStarted),
            e("b", Done),
            e("c", Failed),
            e("d", Begin),
            e("d", Pm2Stopped),
            e("d", VexorStarted),
            e("d", RolledBack),
            e("f", Begin),
            e("f", Pm2Stopped),
        ];
        let r = to_roll_back(&j);
        let names: Vec<&str> = r.iter().map(|(a, _)| a.as_str()).collect();
        assert_eq!(
            names,
            ["f", "b", "a"],
            "newest first; c never begun, d already back"
        );
        assert!(r[0].1.pm2_stopped && !r[1].1.pm2_stopped && r[2].1.pm2_stopped);

        // Rolled back, then applied again: undo the second move.
        let j = [
            e("a", Begin),
            e("a", RolledBack),
            e("a", Begin),
            e("a", Done),
        ];
        assert_eq!(to_roll_back(&j).len(), 1);
    }

    #[test]
    fn journal_last_step_wins_and_torn_lines_are_ignored() {
        let dir = tempfile::tempdir().expect("tmp");
        let mut j = Journal::open(dir.path()).expect("open");
        j.write("a", Step::Begin, None).expect("write");
        j.write("a", Step::Done, None).expect("write");
        j.write("b", Step::Begin, None).expect("write");
        drop(j);
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(dir.path().join(JOURNAL))
            .expect("open");
        f.write_all(b"{\"ts\":\"x\",\"app\":\"b\",\"st")
            .expect("torn");
        let last = last_steps(&read_journal(dir.path()).expect("read"));
        assert_eq!(last["a"], Step::Done);
        assert_eq!(last["b"], Step::Begin);
    }
}

//! `vexor migrate --from-pm2`: plan a move from PM2 to Vexor
//! (docs/migration.md). The dry run reads PM2's `dump.pm2` and changes
//! nothing: no PM2 command is run, no Vexor app is added, and the Vexor
//! daemon is not started.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Serialize;
use serde_json::Value;
use vexor_pm2::dump::{self, DumpApp};
use vexor_pm2::{Context, Note, Severity, Translated};

/// How moving one app affects it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Downtime {
    /// Not running under PM2: nothing to interrupt.
    None,
    /// Stopped under PM2, then started under Vexor: down for about the
    /// app's startup time.
    Brief,
    /// Can't be moved as it is (see the app's errors).
    Blocked,
}

#[derive(Debug, Serialize)]
pub struct AppPlan {
    pub name: String,
    pub pm2_status: Vec<String>,
    pub mode: &'static str,
    pub instances: u32,
    pub downtime: Downtime,
    /// The Vexor spec the app would get (env values redacted).
    pub spec: Option<Value>,
    /// Environment variable names (values are never shown).
    pub env_keys: Vec<String>,
    pub notes: Vec<Note>,
}

#[derive(Debug, Serialize)]
pub struct Report {
    pub dump: PathBuf,
    pub pm2_version: Option<String>,
    /// Seconds since the dump was written.
    pub dump_age_secs: Option<u64>,
    pub processes: usize,
    /// Findings about the PM2 installation as a whole.
    pub warnings: Vec<String>,
    pub apps: Vec<AppPlan>,
}

impl Report {
    pub fn blocked(&self) -> usize {
        self.apps
            .iter()
            .filter(|a| a.downtime == Downtime::Blocked)
            .count()
    }
}

/// `$PM2_HOME`, else `~/.pm2`.
pub fn default_pm2_home() -> PathBuf {
    if let Some(h) = std::env::var_os("PM2_HOME") {
        return PathBuf::from(h);
    }
    std::env::var_os("HOME")
        .map_or_else(|| PathBuf::from(".pm2"), |h| PathBuf::from(h).join(".pm2"))
}

fn current_user() -> Option<String> {
    std::env::var("USER")
        .or_else(|_| std::env::var("LOGNAME"))
        .ok()
}

/// Build the dry-run report from a dump file.
pub fn plan(dump_path: &Path, pm2_home: &Path) -> Result<Report, String> {
    let text = std::fs::read_to_string(dump_path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            format!(
                "no PM2 dump at {}. Run `pm2 save` first so it matches what PM2 is running \
                 (or pass --pm2-home / --dump)",
                dump_path.display()
            )
        } else {
            format!("cannot read {}: {e}", dump_path.display())
        }
    })?;
    let user = current_user();
    let dump =
        dump::parse(&text, user.as_deref()).map_err(|e| format!("{}: {e}", dump_path.display()))?;

    let mut warnings = Vec::new();
    let meta = std::fs::metadata(dump_path).ok();
    let dump_age_secs = meta
        .as_ref()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.elapsed().ok())
        .map(|d| d.as_secs());
    {
        use std::os::unix::fs::PermissionsExt;
        if let Some(m) = &meta
            && m.permissions().mode() & 0o077 != 0
        {
            warnings.push(format!(
                "{} is readable by other users (mode {:o}) and holds every app's environment, \
                 secrets included; consider `chmod 600` it",
                dump_path.display(),
                m.permissions().mode() & 0o777
            ));
        }
    }
    if let Some(unit) = pm2_startup_unit(user.as_deref()) {
        warnings.push(format!(
            "PM2 starts at boot ({}). After migrating, remove it with `pm2 unstartup`, or both \
             would start the apps",
            unit.display()
        ));
    }
    if dump.apps.iter().any(|a| {
        a.app
            .get("env")
            .and_then(Value::as_object)
            .is_some_and(|e| e.contains_key("HOME") && e.contains_key("PATH"))
    }) {
        warnings.push(
            "PM2 saved the shell environment `pm2` was started from with each app's own \
             variables, and can't tell them apart; all of them are kept so apps see what they \
             see under PM2. Check the variable names below"
                .into(),
        );
    }

    let apps = dump.apps.iter().map(|a| plan_app(a, pm2_home)).collect();
    Ok(Report {
        dump: dump_path.to_path_buf(),
        pm2_version: dump.pm2_version,
        dump_age_secs,
        processes: dump.processes,
        warnings,
        apps,
    })
}

fn plan_app(a: &DumpApp, pm2_home: &Path) -> AppPlan {
    let cwd = a
        .app
        .get("cwd")
        .and_then(Value::as_str)
        .map_or_else(|| PathBuf::from("/"), PathBuf::from);
    let ctx = Context {
        config_dir: cwd.clone(),
        env_name: None,
        // The dump already holds the environment PM2 gave the app; the
        // migrating shell's own environment is not added.
        caller_env: BTreeMap::new(),
        inherit_env: false,
        log_dir: pm2_home.join("logs"),
    };
    let mut notes = a.notes.clone();
    let translated = match vexor_pm2::translate(&a.app, &ctx) {
        Ok(t) => {
            notes.extend(t.notes.iter().cloned());
            Some(t)
        }
        Err(n) => {
            notes.extend(n);
            None
        }
    };
    if let Some(t) = &translated {
        notes.extend(check_paths(t, &cwd));
        if let Some(n) = check_node_version(t, a.node_version.as_deref()) {
            notes.push(n);
        }
    }
    // The translator's notes for the generic interpreter warning and this
    // one say the same thing; keep the more precise one.
    if notes
        .iter()
        .any(|n| n.field == "interpreter" && n.message.starts_with("PM2 ran"))
    {
        notes.retain(|n| !(n.field == "interpreter" && n.message.starts_with("PM2 runs")));
    }
    notes.sort_by_key(|n| std::cmp::Reverse(n.severity));
    let blocked = translated.is_none() || notes.iter().any(|n| n.severity == Severity::Fatal);
    let downtime = if blocked {
        Downtime::Blocked
    } else if a.running() {
        Downtime::Brief
    } else {
        Downtime::None
    };
    let (spec, env_keys, instances, mode) = match &translated {
        Some(t) => {
            let mut v = serde_json::to_value(&t.spec).unwrap_or(Value::Null);
            if let Some(env) = v.get_mut("env").and_then(Value::as_object_mut) {
                for val in env.values_mut() {
                    *val = Value::String("<redacted>".into());
                }
            }
            (
                Some(v),
                t.spec.env.keys().cloned().collect(),
                t.spec.instances,
                if t.spec.cluster.is_some() {
                    "cluster"
                } else {
                    "fork"
                },
            )
        }
        None => (None, Vec::new(), 1, "?"),
    };
    AppPlan {
        name: a.name.clone(),
        pm2_status: a.statuses.clone(),
        mode,
        instances,
        downtime,
        spec,
        env_keys,
        notes,
    }
}

fn note(app: &str, field: &str, severity: Severity, message: String) -> Note {
    Note {
        app: app.to_owned(),
        field: field.to_owned(),
        severity,
        message,
    }
}

/// The working directory and script must exist on this machine.
fn check_paths(t: &Translated, cwd: &Path) -> Vec<Note> {
    let name = &t.spec.name;
    let mut out = Vec::new();
    if !cwd.is_dir() {
        out.push(note(
            name,
            "cwd",
            Severity::Fatal,
            format!("{} does not exist", cwd.display()),
        ));
    }
    let script = t
        .spec
        .cluster
        .as_ref()
        .map(|c| c.script.clone())
        .or_else(|| {
            t.spec
                .node_ipc
                .then(|| {
                    t.spec
                        .args
                        .iter()
                        .find(|a| a.starts_with('/'))
                        .map(PathBuf::from)
                })
                .flatten()
        })
        .unwrap_or_else(|| t.spec.exec.clone());
    if !script.exists() {
        out.push(note(
            name,
            "script",
            Severity::Fatal,
            format!("{} does not exist", script.display()),
        ));
    }
    out
}

/// PM2 records the Node.js version it ran the app with; warn when Vexor
/// would use a different major version.
fn check_node_version(t: &Translated, recorded: Option<&str>) -> Option<Note> {
    if !t.spec.node_ipc || !t.spec.exec.is_absolute() {
        return None;
    }
    let recorded = recorded?.trim_start_matches('v');
    let out = Command::new(&t.spec.exec).arg("-v").output().ok()?;
    let found = String::from_utf8_lossy(&out.stdout)
        .trim()
        .trim_start_matches('v')
        .to_owned();
    let major = |v: &str| v.split('.').next().map(str::to_owned);
    if found.is_empty() || major(&found) == major(recorded) {
        return None;
    }
    Some(note(
        &t.spec.name,
        "interpreter",
        Severity::Partial,
        format!(
            "PM2 ran this app on Node.js {recorded}; Vexor would use {} (Node.js {found})",
            t.spec.exec.display()
        ),
    ))
}

/// PM2's boot service, if installed (`pm2 startup`).
fn pm2_startup_unit(user: Option<&str>) -> Option<PathBuf> {
    let user = user?;
    let mut candidates = vec![PathBuf::from(format!(
        "/etc/systemd/system/pm2-{user}.service"
    ))];
    if let Some(home) = std::env::var_os("HOME") {
        candidates.push(
            PathBuf::from(home)
                .join("Library/LaunchAgents")
                .join(format!("pm2.{user}.plist")),
        );
    }
    candidates.into_iter().find(|p| p.exists())
}

// ------------------------------------------------------------------ output

fn age(secs: u64) -> String {
    match secs {
        s if s < 120 => format!("{s} seconds ago"),
        s if s < 7200 => format!("{} minutes ago", s / 60),
        s if s < 172_800 => format!("{} hours ago", s / 3600),
        s => format!("{} days ago", s / 86400),
    }
}

/// Quote an argument for display when it would otherwise be ambiguous.
fn quote(a: &str) -> String {
    if !a.is_empty() && !a.contains(|c: char| c.is_whitespace() || "'\"\\$`".contains(c)) {
        a.to_owned()
    } else {
        format!("'{}'", a.replace('\'', "'\\''"))
    }
}

pub fn print(r: &Report) {
    println!(
        "PM2 dump: {} ({}{} processes, {} apps{})",
        r.dump.display(),
        r.pm2_version
            .as_deref()
            .map(|v| format!("PM2 {v}, "))
            .unwrap_or_default(),
        r.processes,
        r.apps.len(),
        r.dump_age_secs
            .map(|s| format!(", saved {}", age(s)))
            .unwrap_or_default(),
    );
    for w in &r.warnings {
        println!("warning: {w}");
    }
    println!();
    let mut rows = vec![[
        "APP".to_owned(),
        "MODE".to_owned(),
        "INSTANCES".to_owned(),
        "PM2 STATUS".to_owned(),
        "DOWNTIME".to_owned(),
    ]];
    for a in &r.apps {
        let mut st = a.pm2_status.clone();
        st.dedup();
        rows.push([
            a.name.clone(),
            a.mode.to_owned(),
            a.instances.to_string(),
            st.join(","),
            match a.downtime {
                Downtime::None => "none (not running)",
                Downtime::Brief => "brief (stop, then start)",
                Downtime::Blocked => "blocked",
            }
            .to_owned(),
        ]);
    }
    crate::print_rows(&rows);

    for a in &r.apps {
        println!("\n{}", a.name);
        if let Some(s) = &a.spec {
            let args: Vec<String> = s["args"]
                .as_array()
                .map(|v| v.iter().filter_map(Value::as_str).map(quote).collect())
                .unwrap_or_default();
            println!(
                "  run:  {} {}",
                s["exec"].as_str().unwrap_or("?"),
                args.join(" ")
            );
            println!("  cwd:  {}", s["cwd"].as_str().unwrap_or("?"));
            println!(
                "  logs: {}, {}",
                s["logs"]["out"].as_str().unwrap_or("?"),
                s["logs"]["err"].as_str().unwrap_or("?")
            );
        }
        if !a.env_keys.is_empty() {
            println!(
                "  env:  {} variables (values not shown): {}",
                a.env_keys.len(),
                a.env_keys.join(", ")
            );
        }
        for n in &a.notes {
            let level = match n.severity {
                Severity::Fatal => "error",
                Severity::Partial => "warning",
                Severity::Ignored => "note",
            };
            println!("  {level}: {}: {}", n.field, n.message);
        }
    }

    let blocked = r.blocked();
    println!();
    if r.apps.is_empty() {
        println!("The dump has no apps.");
    } else if blocked == 0 {
        println!("All {} apps can be migrated.", r.apps.len());
    } else {
        println!(
            "{} of {} apps can be migrated; {blocked} blocked by the errors above.",
            r.apps.len() - blocked,
            r.apps.len()
        );
    }
    println!(
        "Dry run: nothing was changed. Each running app is stopped under PM2 and started under \
         Vexor in turn, so expect a gap of about its startup time; for no gap, move hosts behind \
         a load balancer one at a time (docs/migration.md)."
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arguments_are_quoted_when_needed() {
        assert_eq!(quote("--flag"), "--flag");
        assert_eq!(quote("high prio"), "'high prio'");
        assert_eq!(quote("it's"), "'it'\\''s'");
        assert_eq!(quote(""), "''");
    }

    #[test]
    fn ages_read_naturally() {
        assert_eq!(age(5), "5 seconds ago");
        assert_eq!(age(600), "10 minutes ago");
        assert_eq!(age(7200), "2 hours ago");
        assert_eq!(age(3 * 86400), "3 days ago");
    }
}

// ------------------------------------------------------------------ backup

/// Files copied from the PM2 home, when present. `dump.pm2` is required.
const PM2_FILES: &[&str] = &["dump.pm2", "dump.pm2.bak", "module_conf.json", "conf.js"];

/// Largest file a backup copies (PM2's state files are a few KiB).
const MAX_FILE: u64 = 64 << 20;

#[derive(Debug, Serialize)]
pub struct BackedUpFile {
    pub source: PathBuf,
    /// Path inside the backup directory.
    pub copy: String,
    pub size: u64,
    /// Permission bits of the original.
    pub mode: u32,
    pub sha256: String,
}

#[derive(Debug, Serialize)]
pub struct Manifest {
    pub id: String,
    pub created_at: String,
    pub vexor_version: &'static str,
    pub pm2_home: PathBuf,
    pub dump: PathBuf,
    pub files: Vec<BackedUpFile>,
    /// SHA-256 of `plan.json`, the dry-run report saved with the backup.
    pub plan_sha256: String,
}

pub fn migrations_dir(home: &Path) -> PathBuf {
    home.join("migrations")
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

pub fn sha256(data: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    hex(&Sha256::digest(data))
}

/// Read a regular file without following a symlink at its final component.
fn read_regular(path: &Path) -> std::io::Result<(Vec<u8>, u32)> {
    use std::io::Read;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    let mut f = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc_o_nofollow())
        .open(path)?;
    let meta = f.metadata()?;
    if !meta.is_file() {
        return Err(std::io::Error::other("not a regular file"));
    }
    if meta.len() > MAX_FILE {
        return Err(std::io::Error::other(format!(
            "larger than {} MiB",
            MAX_FILE >> 20
        )));
    }
    let mut buf = Vec::new();
    f.read_to_end(&mut buf)?;
    Ok((buf, meta.mode() & 0o7777))
}

fn libc_o_nofollow() -> i32 {
    rustix::fs::OFlags::NOFOLLOW.bits().cast_signed()
}

/// Write `data` to a new file readable only by the owner.
fn write_private(path: &Path, data: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    f.write_all(data)?;
    f.sync_all()
}

/// Create a directory readable only by the owner; fails if it exists.
fn private_new_dir(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new().mode(0o700).create(path)
}

/// Copy PM2's state and the migration plan to a new directory under
/// `<home>/migrations/`. Never overwrites: each backup gets its own id. The
/// directory appears only once complete.
pub fn backup(
    home: &Path,
    pm2_home: &Path,
    dump: &Path,
    report: &Report,
    now_ms: u64,
) -> Result<(PathBuf, Manifest), String> {
    let err = |what: &str, p: &Path, e: std::io::Error| format!("{what} {}: {e}", p.display());
    let root = migrations_dir(home);
    if !home.exists() {
        std::fs::DirBuilder::new()
            .recursive(true)
            .create(home)
            .map_err(|e| err("cannot create", home, e))?;
        std::fs::set_permissions(home, std::os::unix::fs::PermissionsExt::from_mode(0o700))
            .map_err(|e| err("cannot restrict", home, e))?;
    }
    match private_new_dir(&root) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(err("cannot create", &root, e)),
    }

    let stamp = crate::format_utc(now_ms);
    // "2026-10-23 05:30:12.345" -> "20261023-053012"
    let digits: String = stamp
        .chars()
        .take(19)
        .filter(char::is_ascii_digit)
        .collect();
    let base = format!("{}-{}", &digits[..8], &digits[8..]);
    let mut id = base.clone();
    let mut n = 1;
    while root.join(&id).exists() || root.join(format!(".{id}.tmp")).exists() {
        n += 1;
        id = format!("{base}-{n}");
    }
    let tmp = root.join(format!(".{id}.tmp"));
    private_new_dir(&tmp).map_err(|e| err("cannot create", &tmp, e))?;
    let result = (|| {
        private_new_dir(&tmp.join("pm2")).map_err(|e| err("cannot create", &tmp, e))?;
        let mut files = Vec::new();
        let mut sources: Vec<(PathBuf, String)> = PM2_FILES
            .iter()
            .map(|f| (pm2_home.join(f), format!("pm2/{f}")))
            .collect();
        // A dump from elsewhere (--dump) is backed up alone: the PM2
        // home's other files belong to a different dump.
        if dump != pm2_home.join("dump.pm2") {
            sources = vec![(dump.to_path_buf(), "pm2/dump.pm2".into())];
        }
        if let Some(unit) = pm2_startup_unit(current_user().as_deref()) {
            let name = unit
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            sources.push((unit, format!("startup/{name}")));
        }
        for (source, copy) in sources {
            let (data, mode) = match read_regular(&source) {
                Ok(x) => x,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound && copy != "pm2/dump.pm2" => {
                    continue;
                }
                Err(e) => return Err(err("cannot read", &source, e)),
            };
            let dest = tmp.join(&copy);
            if let Some(parent) = dest.parent()
                && !parent.exists()
            {
                private_new_dir(parent).map_err(|e| err("cannot create", parent, e))?;
            }
            write_private(&dest, &data).map_err(|e| err("cannot write", &dest, e))?;
            files.push(BackedUpFile {
                source,
                copy,
                size: data.len() as u64,
                mode,
                sha256: sha256(&data),
            });
        }
        let plan = serde_json::to_vec_pretty(report).map_err(|e| e.to_string())?;
        write_private(&tmp.join("plan.json"), &plan)
            .map_err(|e| err("cannot write", &tmp.join("plan.json"), e))?;
        let manifest = Manifest {
            id: id.clone(),
            created_at: format!("{stamp} UTC"),
            vexor_version: env!("CARGO_PKG_VERSION"),
            pm2_home: pm2_home.to_path_buf(),
            dump: dump.to_path_buf(),
            files,
            plan_sha256: sha256(&plan),
        };
        let m = serde_json::to_vec_pretty(&manifest).map_err(|e| e.to_string())?;
        write_private(&tmp.join("manifest.json"), &m)
            .map_err(|e| err("cannot write", &tmp.join("manifest.json"), e))?;
        Ok(manifest)
    })();
    match result {
        Ok(manifest) => {
            let dir = root.join(&id);
            std::fs::rename(&tmp, &dir).map_err(|e| err("cannot finish", &dir, e))?;
            if let Ok(d) = std::fs::File::open(&root) {
                let _ = d.sync_all();
            }
            Ok((dir, manifest))
        }
        Err(e) => {
            let _ = std::fs::remove_dir_all(&tmp);
            Err(e)
        }
    }
}

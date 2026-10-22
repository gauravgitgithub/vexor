//! Desired state: `$VEXOR_HOME/state.json` (docs/architecture.md §8).
//!
//! The authoritative record of which apps exist, their specs, and whether
//! they should run. Rewritten atomically as a whole on every change. A
//! corrupt or newer-version file stops the daemon instead of being replaced.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use vexor_core::AppSpec;
use vexor_proto::Desired;

pub const VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppEntry {
    pub spec: AppSpec,
    pub desired: Desired,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    version: u32,
    apps: BTreeMap<String, AppEntry>,
}

#[derive(Debug)]
pub struct Store {
    path: PathBuf,
    apps: BTreeMap<String, AppEntry>,
}

impl Store {
    /// Load `path`, or start empty if it doesn't exist.
    pub fn load(path: &Path) -> Result<Store, String> {
        let apps = match std::fs::read(path) {
            Ok(bytes) => {
                let file: File = serde_json::from_slice(&bytes).map_err(|e| {
                    format!(
                        "{} is unreadable ({e}). It was left untouched; fix or move it, then start the daemon again",
                        path.display()
                    )
                })?;
                if file.version != VERSION {
                    return Err(format!(
                        "{} has version {}, this vexor supports {VERSION}. It was left untouched",
                        path.display(),
                        file.version
                    ));
                }
                file.apps
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => BTreeMap::new(),
            Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
        };
        Ok(Store {
            path: path.to_path_buf(),
            apps,
        })
    }

    pub fn apps(&self) -> &BTreeMap<String, AppEntry> {
        &self.apps
    }

    pub fn get(&self, name: &str) -> Option<&AppEntry> {
        self.apps.get(name)
    }

    /// Apply `change` and persist. On a write error nothing changes in memory.
    pub fn update(
        &mut self,
        change: impl FnOnce(&mut BTreeMap<String, AppEntry>),
    ) -> io::Result<()> {
        let mut next = self.apps.clone();
        change(&mut next);
        let file = File {
            version: VERSION,
            apps: next,
        };
        let json = serde_json::to_vec_pretty(&file).map_err(io::Error::other)?;
        vexor_shim::write_atomic(&self.path, &json)?;
        self.apps = file.apps;
        Ok(())
    }
}

/// `vexor save` snapshot: every app and its desired state.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Dump {
    version: u32,
    saved_ms: u64,
    pub apps: BTreeMap<String, AppEntry>,
}

impl Dump {
    pub fn of(apps: &BTreeMap<String, AppEntry>) -> Dump {
        Dump {
            version: VERSION,
            saved_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(0)),
            apps: apps.clone(),
        }
    }

    pub fn write(&self, path: &Path) -> io::Result<()> {
        let json = serde_json::to_vec_pretty(self).map_err(io::Error::other)?;
        vexor_shim::write_atomic(path, &json)
    }

    /// `Ok(None)` if nothing was saved.
    pub fn read(path: &Path) -> Result<Option<Dump>, String> {
        match std::fs::read(path) {
            Ok(bytes) => {
                let d: Dump = serde_json::from_slice(&bytes)
                    .map_err(|e| format!("{} is unreadable: {e}", path.display()))?;
                if d.version != VERSION {
                    return Err(format!(
                        "{} has version {}, this vexor supports {VERSION}",
                        path.display(),
                        d.version
                    ));
                }
                Ok(Some(d))
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(format!("cannot read {}: {e}", path.display())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(name: &str) -> AppSpec {
        serde_json::from_value(serde_json::json!({
            "name": name, "exec": "/bin/true", "cwd": "/",
            "logs": { "out": "/tmp/o", "err": "/tmp/e" },
        }))
        .expect("spec")
    }

    #[test]
    fn persists_across_loads() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("state.json");
        let mut s = Store::load(&path).expect("empty");
        s.update(|apps| {
            apps.insert(
                "a".into(),
                AppEntry {
                    spec: spec("a"),
                    desired: Desired::Stopped,
                },
            );
        })
        .expect("write");
        let again = Store::load(&path).expect("load");
        assert_eq!(again.get("a").map(|e| e.desired), Some(Desired::Stopped));
    }

    #[test]
    fn corrupt_or_newer_files_are_refused_and_kept() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("state.json");
        for bad in [&b"{not json"[..], br#"{"version":99,"apps":{}}"#] {
            std::fs::write(&path, bad).expect("write");
            let err = Store::load(&path).expect_err("must refuse");
            assert!(err.contains("untouched"), "{err}");
            assert_eq!(std::fs::read(&path).expect("read"), bad);
        }
    }
}

//! The daemon's own JSON-lines log: `$VEXOR_HOME/daemon.log`, mirrored to
//! stdout.

use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

#[derive(Debug, Clone)]
pub struct Log(Arc<Mutex<std::fs::File>>);

impl Log {
    pub fn open(path: &Path) -> std::io::Result<Log> {
        let f = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)?;
        Ok(Log(Arc::new(Mutex::new(f))))
    }

    pub fn write(&self, mut entry: Value) {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_millis());
        if let Value::Object(m) = &mut entry {
            m.insert("ts_ms".into(), json!(ts));
        }
        let line = format!("{entry}\n");
        let mut f = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        // Logging must never stop the daemon.
        let _ = f.write_all(line.as_bytes());
        let mut out = std::io::stdout().lock();
        let _ = out.write_all(line.as_bytes());
        let _ = out.flush();
    }
}

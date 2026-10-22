//! Lifecycle events, written as one JSON object per line on stdout.
//!
//! This is the shim's only output channel until the daemon protocol exists
//! (plan step 2.4b); tests and `vexor shim` users read it directly.

use std::io::Write;
use std::path::Path;

use serde::Serialize;
use vexor_core::{ErrorReason, ExitStatus, State};

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ShimEvent<'a> {
    State {
        app: &'a str,
        instance: u32,
        state: State,
        #[serde(skip_serializing_if = "Option::is_none")]
        pid: Option<u32>,
        #[serde(skip_serializing_if = "Option::is_none")]
        start: Option<u64>,
        restarts: u64,
        #[serde(skip_serializing_if = "Option::is_none")]
        exit: Option<ExitStatus>,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<ErrorReason>,
    },
    /// Processes were still in the instance's group after its main process
    /// exited, and were sent `signal`.
    GroupCleanup {
        app: &'a str,
        instance: u32,
        signal: &'static str,
    },
    /// A descendant escaped its process group and was adopted by the shim
    /// (Linux). It is stopped when the shim shuts down.
    Orphan { app: &'a str, pid: u32, start: u64 },
    SpawnError {
        app: &'a str,
        instance: u32,
        exec: &'a Path,
        error: String,
    },
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Output;

impl Output {
    pub fn emit(&self, event: &ShimEvent<'_>) {
        // Serializing these types can't fail; a closed stdout must not stop
        // supervision, so write errors are ignored.
        if let Ok(line) = serde_json::to_string(event) {
            let mut out = std::io::stdout().lock();
            let _ = writeln!(out, "{line}");
            let _ = out.flush();
        }
    }
}

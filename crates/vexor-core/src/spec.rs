//! Fully resolved description of one app, as handed to a shim.
//!
//! Everything here is already resolved by the CLI: absolute executable path,
//! absolute cwd, the complete environment. The shim adds nothing implicit
//! except the per-instance variable (`instance_var`) and, for Node apps,
//! `NODE_CHANNEL_FD`.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::lifecycle::{HealthPolicy, InstancePolicy};
use crate::restart::{Backoff, Jitter, RestartPolicy};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppSpec {
    pub name: String,
    /// Absolute path of the program to execute (the interpreter, for scripts).
    pub exec: PathBuf,
    /// Arguments after `exec`, passed as-is: never split or shell-expanded.
    #[serde(default)]
    pub args: Vec<String>,
    /// Absolute working directory.
    pub cwd: PathBuf,
    /// The complete environment. Nothing is inherited from the shim.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default = "one")]
    pub instances: u32,
    /// PM2 `instance_var`: set to the instance index in each instance's env.
    #[serde(default = "default_instance_var")]
    pub instance_var: String,
    /// Give the app a Node IPC channel (`NODE_CHANNEL_FD`) so
    /// `process.send('ready')` works.
    #[serde(default)]
    pub node_ipc: bool,
    /// PM2 `treekill`: signal the instance's whole process group, and clean
    /// up processes left in it after the main process exits. When false,
    /// only the main process is signalled.
    #[serde(default = "yes")]
    pub treekill: bool,
    pub logs: LogSpec,
    #[serde(default)]
    pub policy: PolicySpec,
    /// PM2 cluster mode: `exec` is Node, which runs Vexor's cluster primary;
    /// the primary forks `instances` workers running `script`, sharing
    /// listening sockets. `args` above is not used.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cluster: Option<ClusterSpec>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClusterSpec {
    /// Absolute path of the worker script.
    pub script: PathBuf,
    /// Node options for workers (PM2 `node_args`).
    #[serde(default)]
    pub node_args: Vec<String>,
    /// Arguments for workers (PM2 `args`).
    #[serde(default)]
    pub args: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LogSpec {
    pub out: PathBuf,
    pub err: PathBuf,
    /// Rotate a log file once it would grow past this size. 0 disables
    /// rotation (the file grows without bound).
    #[serde(default = "default_max_size")]
    pub max_size_bytes: u64,
    /// Rotated files to keep (`out.log.1` … `out.log.N`). With 0, the file is
    /// truncated instead of rotated.
    #[serde(default = "default_retain")]
    pub retain: u32,
    /// What to do when output arrives faster than it can be written.
    #[serde(default)]
    pub overflow: Overflow,
    /// Output buffered per log file before `overflow` applies.
    #[serde(default = "default_buffer")]
    pub buffer_bytes: u32,
}

impl LogSpec {
    /// Log files at `out` and `err` with default rotation and buffering.
    pub fn new(out: PathBuf, err: PathBuf) -> LogSpec {
        LogSpec {
            out,
            err,
            max_size_bytes: default_max_size(),
            retain: default_retain(),
            overflow: Overflow::default(),
            buffer_bytes: default_buffer(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Overflow {
    /// Discard the excess, count it, and say so in the log and in an event.
    /// The app never waits for the disk.
    #[default]
    Drop,
    /// Stop reading the app's output until there is room: the app blocks on
    /// writes to stdout/stderr. Nothing is lost.
    Block,
}

fn default_max_size() -> u64 {
    10 * 1024 * 1024
}

fn default_retain() -> u32 {
    5
}

fn default_buffer() -> u32 {
    1024 * 1024
}

/// Wire form of [`InstancePolicy`]; durations in milliseconds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct PolicySpec {
    pub autorestart: bool,
    pub restart_delay_ms: u64,
    /// When set, exponential backoff starting here (PM2
    /// `exp_backoff_restart_delay`) replaces `restart_delay_ms`.
    pub exp_backoff_initial_ms: Option<u64>,
    pub exp_backoff_max_ms: u64,
    pub jitter: bool,
    pub min_uptime_ms: u64,
    pub max_restarts: u32,
    pub stop_exit_codes: Vec<i32>,
    pub ready_timeout_ms: Option<u64>,
    pub kill_timeout_ms: u64,
    pub health_unhealthy_after: Option<u32>,
    pub health_restart_after: Option<u32>,
}

impl Default for PolicySpec {
    fn default() -> Self {
        let p = InstancePolicy::default();
        let r = &p.restart;
        PolicySpec {
            autorestart: r.autorestart,
            restart_delay_ms: 0,
            exp_backoff_initial_ms: None,
            exp_backoff_max_ms: 15_000,
            jitter: false,
            min_uptime_ms: millis(r.min_uptime),
            max_restarts: r.max_restarts,
            stop_exit_codes: Vec::new(),
            ready_timeout_ms: None,
            kill_timeout_ms: millis(p.kill_timeout),
            health_unhealthy_after: None,
            health_restart_after: None,
        }
    }
}

impl PolicySpec {
    pub fn to_policy(&self) -> InstancePolicy {
        let ms = Duration::from_millis;
        let backoff = match self.exp_backoff_initial_ms {
            Some(initial) => Backoff::Exponential {
                initial: ms(initial),
                max: ms(self.exp_backoff_max_ms),
            },
            None => Backoff::Fixed(ms(self.restart_delay_ms)),
        };
        let health = match (self.health_unhealthy_after, self.health_restart_after) {
            (None, None) => None,
            (u, r) => {
                let restart_after = r.unwrap_or(u32::MAX);
                Some(HealthPolicy {
                    unhealthy_after: u.unwrap_or(restart_after),
                    restart_after,
                })
            }
        };
        InstancePolicy {
            restart: RestartPolicy {
                autorestart: self.autorestart,
                backoff,
                jitter: if self.jitter {
                    Jitter::Equal
                } else {
                    Jitter::None
                },
                min_uptime: ms(self.min_uptime_ms),
                max_restarts: self.max_restarts,
                stop_exit_codes: self.stop_exit_codes.clone(),
                ..RestartPolicy::default()
            },
            ready_timeout: self.ready_timeout_ms.map(ms),
            kill_timeout: ms(self.kill_timeout_ms),
            health,
        }
    }
}

fn yes() -> bool {
    true
}

fn one() -> u32 {
    1
}

fn default_instance_var() -> String {
    "NODE_APP_INSTANCE".to_owned()
}

fn millis(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_policy_spec_round_trips_to_default_policy() {
        assert_eq!(PolicySpec::default().to_policy(), InstancePolicy::default());
    }

    #[test]
    fn unknown_fields_are_rejected() {
        let json =
            r#"{"name":"a","exec":"/bin/true","cwd":"/","logs":{"out":"/o","err":"/e"},"bogus":1}"#;
        let err = serde_json::from_str::<AppSpec>(json).expect_err("must reject");
        assert!(err.to_string().contains("bogus"), "{err}");
    }

    #[test]
    fn minimal_spec_gets_defaults() {
        let json = r#"{"name":"a","exec":"/bin/true","cwd":"/","logs":{"out":"/o","err":"/e"}}"#;
        let spec: AppSpec = serde_json::from_str(json).expect("valid");
        assert_eq!(spec.instances, 1);
        assert_eq!(spec.instance_var, "NODE_APP_INSTANCE");
        assert!(spec.treekill);
        assert!(spec.env.is_empty());
        assert_eq!(spec.policy, PolicySpec::default());
        assert_eq!(spec.logs.max_size_bytes, 10 * 1024 * 1024);
        assert_eq!(spec.logs.retain, 5);
        assert_eq!(spec.logs.overflow, Overflow::Drop);
    }

    #[test]
    fn exponential_backoff_spec() {
        let spec = PolicySpec {
            exp_backoff_initial_ms: Some(100),
            ..PolicySpec::default()
        };
        assert_eq!(
            spec.to_policy().restart.backoff,
            Backoff::Exponential {
                initial: Duration::from_millis(100),
                max: Duration::from_millis(15_000)
            }
        );
    }
}

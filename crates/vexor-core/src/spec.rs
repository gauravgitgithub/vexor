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

use crate::lifecycle::{HealthPolicy, InstancePolicy, ReplacementReady};
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
    /// PM2 `cron_restart`: restart every instance on this schedule (5 or 6
    /// cron fields, local time). Also starts instances that have stopped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cron_restart: Option<String>,
    /// PM2 `watch`: restart every instance when files change.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub watch: Option<WatchSpec>,
    /// Periodic HTTP or TCP check. Failures count towards
    /// `policy.health_unhealthy_after` / `health_restart_after`; a pass while
    /// starting counts as ready.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub health_check: Option<HealthCheckSpec>,
}

/// A health check against the app. Only loopback addresses are allowed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HealthCheckSpec {
    /// `http://127.0.0.1:3000/health`: passes on a 2xx or 3xx status.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http: Option<String>,
    /// `127.0.0.1:3000`: passes when a connection is accepted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tcp: Option<String>,
    #[serde(default = "default_health_interval")]
    pub interval_ms: u64,
    #[serde(default = "default_health_timeout")]
    pub timeout_ms: u64,
}

/// Where a health check connects, after `{instance}` substitution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HealthTarget {
    pub host: String,
    pub port: u16,
    /// HTTP request path; `None` for a TCP check.
    pub path: Option<String>,
}

impl HealthCheckSpec {
    /// The target for one instance (`{instance}` is replaced by its index;
    /// `None`: the template must not use it). Errors describe what's wrong.
    pub fn target(&self, instance: Option<u32>) -> Result<HealthTarget, String> {
        let (raw, http) = match (&self.http, &self.tcp) {
            (Some(u), None) => (u.as_str(), true),
            (None, Some(a)) => (a.as_str(), false),
            _ => return Err("health_check needs exactly one of `http` and `tcp`".into()),
        };
        let raw = match instance {
            Some(i) => raw.replace("{instance}", &i.to_string()),
            None if raw.contains("{instance}") => {
                return Err(
                    "{instance} can't be used here: in cluster mode every instance shares the app's port"
                        .into(),
                );
            }
            None => raw.to_owned(),
        };
        let (hostport, path) = if http {
            let rest = raw.strip_prefix("http://").ok_or_else(|| {
                format!("health_check.http must start with http:// (https is not supported), got {raw:?}")
            })?;
            match rest.find('/') {
                Some(i) => (rest[..i].to_owned(), Some(rest[i..].to_owned())),
                None => (rest.to_owned(), Some("/".to_owned())),
            }
        } else {
            (raw.clone(), None)
        };
        let (host, port) = hostport
            .rsplit_once(':')
            .ok_or_else(|| format!("health check address needs a port: {raw:?}"))?;
        let port: u16 = port
            .parse()
            .ok()
            .filter(|p| *p > 0)
            .ok_or_else(|| format!("invalid port in health check address {raw:?}"))?;
        let host = host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .to_owned();
        let loopback = host == "localhost"
            || host
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback());
        if !loopback {
            return Err(format!(
                "health checks may only target this machine (127.0.0.1, ::1 or localhost), got {host:?}"
            ));
        }
        if path
            .as_deref()
            .is_some_and(|p| p.contains(char::is_whitespace))
        {
            return Err(format!("invalid health check path in {raw:?}"));
        }
        Ok(HealthTarget { host, port, path })
    }
}

fn default_health_interval() -> u64 {
    10_000
}

fn default_health_timeout() -> u64 {
    2_000
}

/// What `watch` watches.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WatchSpec {
    /// Absolute files or directories (directories recursively).
    pub paths: Vec<PathBuf>,
    /// Glob patterns of changes to ignore, matched against the path relative
    /// to the watched directory, each directory above it, and each single
    /// path component. The app's own log files are always ignored.
    #[serde(default)]
    pub ignore: Vec<String>,
    /// Wait this long after the first change before restarting (PM2
    /// `watch_delay`).
    #[serde(default)]
    pub delay_ms: u64,
}

impl WatchSpec {
    /// PM2's default `ignore_watch`: dot-files and `node_modules`.
    pub fn default_ignore() -> Vec<String> {
        vec![".*".to_owned(), "node_modules".to_owned()]
    }
}

impl AppSpec {
    /// The lifecycle policy for this app's instances, including how
    /// `reload` works (docs/architecture.md §6a).
    pub fn instance_policy(&self) -> InstancePolicy {
        let mut p = self.policy.to_policy();
        let start_first = match self.policy.reload_strategy {
            ReloadStrategy::Auto => self.cluster.is_some(),
            ReloadStrategy::StartFirst => true,
            ReloadStrategy::StopFirst => false,
        };
        if start_first {
            p.reload = Some(match p.ready_timeout {
                Some(t) => ReplacementReady::Signal(t),
                None if self.cluster.is_some() => ReplacementReady::Signal(
                    self.policy
                        .reload_timeout_ms
                        .map_or(DEFAULT_RELOAD_TIMEOUT, Duration::from_millis),
                ),
                None => ReplacementReady::Stable(p.restart.min_uptime.max(Duration::from_secs(1))),
            });
        }
        p
    }
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
    /// Start each plain-text line with the local time it was received
    /// (ISO 8601 with milliseconds and UTC offset). Lines that start with
    /// `{` (JSON logs, which carry their own time) are left as they are.
    #[serde(default = "yes")]
    pub timestamps: bool,
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
            timestamps: true,
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
    /// How `reload` replaces processes.
    pub reload_strategy: ReloadStrategy,
    /// Reload: how long a replacement may take to become ready (cluster
    /// mode without `ready_timeout_ms`, where "listening" counts as ready).
    pub reload_timeout_ms: Option<u64>,
    /// Signal for a graceful stop (stop, restart, retiring after reload).
    /// SIGINT by default, as PM2.
    pub kill_signal: KillSignal,
    /// Restart an instance whose resident memory (RSS) is above this many
    /// bytes when checked (PM2 `max_memory_restart`).
    pub max_memory_bytes: Option<u64>,
    /// How often memory is checked while `max_memory_bytes` is set.
    pub memory_check_interval_ms: u64,
    /// How often the shim samples CPU, memory and open files of each
    /// instance for `list`/`describe`; 0 turns sampling off.
    pub metrics_interval_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum KillSignal {
    #[default]
    #[serde(rename = "SIGINT")]
    Int,
    #[serde(rename = "SIGTERM")]
    Term,
    #[serde(rename = "SIGHUP")]
    Hup,
    #[serde(rename = "SIGQUIT")]
    Quit,
    #[serde(rename = "SIGUSR1")]
    Usr1,
    #[serde(rename = "SIGUSR2")]
    Usr2,
}

impl KillSignal {
    pub fn name(self) -> &'static str {
        match self {
            KillSignal::Int => "SIGINT",
            KillSignal::Term => "SIGTERM",
            KillSignal::Hup => "SIGHUP",
            KillSignal::Quit => "SIGQUIT",
            KillSignal::Usr1 => "SIGUSR1",
            KillSignal::Usr2 => "SIGUSR2",
        }
    }

    pub fn parse(s: &str) -> Option<KillSignal> {
        let s = s.trim().to_ascii_uppercase();
        let s = s.strip_prefix("SIG").unwrap_or(&s);
        Some(match s {
            "INT" => KillSignal::Int,
            "TERM" => KillSignal::Term,
            "HUP" => KillSignal::Hup,
            "QUIT" => KillSignal::Quit,
            "USR1" => KillSignal::Usr1,
            "USR2" => KillSignal::Usr2,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReloadStrategy {
    /// Start-first in cluster mode, stop-first in fork mode (as PM2).
    #[default]
    Auto,
    /// Start the replacement, wait until it is healthy, then stop the old
    /// process. In fork mode this needs an app that can run twice at once
    /// (no fixed port, or `reusePort`).
    StartFirst,
    /// Stop, then start: a restart.
    StopFirst,
}

/// Reload readiness deadline in cluster mode when nothing else is set.
const DEFAULT_RELOAD_TIMEOUT: Duration = Duration::from_secs(30);

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
            reload_strategy: ReloadStrategy::Auto,
            reload_timeout_ms: None,
            kill_signal: KillSignal::Int,
            max_memory_bytes: None,
            memory_check_interval_ms: 5_000,
            metrics_interval_ms: 5_000,
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
            reload: None,
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

#[cfg(test)]
mod health_tests {
    use super::*;

    fn http(u: &str) -> HealthCheckSpec {
        HealthCheckSpec {
            http: Some(u.into()),
            tcp: None,
            interval_ms: 1000,
            timeout_ms: 500,
        }
    }

    #[test]
    fn targets_parse_and_substitute() {
        let t = http("http://127.0.0.1:30{instance}1/health?x=1")
            .target(Some(2))
            .expect("ok");
        assert_eq!(
            (t.host.as_str(), t.port, t.path.as_deref()),
            ("127.0.0.1", 3021, Some("/health?x=1"))
        );
        let t = http("http://localhost:8080").target(Some(0)).expect("ok");
        assert_eq!(t.path.as_deref(), Some("/"));
        let t = http("http://[::1]:9000/h").target(None).expect("ok");
        assert_eq!(t.host, "::1");
        let tcp = HealthCheckSpec {
            http: None,
            tcp: Some("127.0.0.1:6379".into()),
            ..http("")
        };
        assert_eq!(tcp.target(None).expect("ok").path, None);
    }

    #[test]
    fn bad_targets_are_errors() {
        for u in [
            "https://127.0.0.1:1/",
            "http://10.0.0.5:80/",
            "http://example.com:80/",
            "http://127.0.0.1/",
            "http://127.0.0.1:0/",
            "http://127.0.0.1:99999/",
            "127.0.0.1:80",
        ] {
            assert!(http(u).target(Some(0)).is_err(), "{u}");
        }
        assert!(
            http("http://127.0.0.1:30{instance}0/")
                .target(None)
                .is_err()
        );
        let both = HealthCheckSpec {
            tcp: Some("127.0.0.1:1".into()),
            ..http("http://127.0.0.1:1/")
        };
        assert!(both.target(None).is_err());
    }
}

//! `vexor describe`: one app's configuration, live status and recent events.

use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;
use vexor_core::{AppSpec, ReloadStrategy, ReplacementReady};
use vexor_proto::{AppStatus, Desired};

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(0))
}

/// "3d 4h", "5m 2s", "12s".
pub fn human_duration(ms: u64) -> String {
    let s = ms / 1000;
    let (d, h, m, sec) = (s / 86_400, s % 86_400 / 3600, s % 3600 / 60, s % 60);
    match (d, h, m) {
        (0, 0, 0) => format!("{sec}s"),
        (0, 0, _) => format!("{m}m {sec}s"),
        (0, _, _) => format!("{h}h {m}m"),
        _ => format!("{d}d {h}h"),
    }
}

/// How long an instance has been in its current state, if it is online.
pub fn uptime(snapshot: &Value, now: u64) -> Option<String> {
    (snapshot["state"] == "online")
        .then(|| snapshot["ts_ms"].as_u64())
        .flatten()
        .map(|t| human_duration(now.saturating_sub(t)))
}

/// The spec as JSON, with environment values replaced unless `show_env`.
pub fn spec_json(spec: &AppSpec, show_env: bool) -> Value {
    let mut v = serde_json::to_value(spec).unwrap_or(Value::Null);
    if !show_env && let Some(env) = v.get_mut("env").and_then(Value::as_object_mut) {
        for val in env.values_mut() {
            *val = Value::String("<redacted>".into());
        }
    }
    v
}

fn args_text(args: &[String]) -> String {
    if args.is_empty() {
        "(none)".into()
    } else {
        args.join(" ")
    }
}

fn ms(v: u64) -> String {
    if v >= 1000 && v.is_multiple_of(1000) {
        format!("{}s", v / 1000)
    } else {
        format!("{v}ms")
    }
}

pub fn print(status: &AppStatus, spec: &AppSpec, events: &[Value], show_env: bool) {
    let now = now_ms();
    let row = |k: &str, v: String| println!("{k:<13}{v}");
    row("Name", spec.name.clone());
    let desired = match status.desired {
        Desired::Running => "running",
        Desired::Stopped => "stopped",
    };
    row(
        "Desired",
        match status.shim_pid {
            Some(p) => format!("{desired} (shim pid {p})"),
            None => format!("{desired} (no shim running)"),
        },
    );
    let mode = if spec.cluster.is_some() {
        "cluster"
    } else {
        "fork"
    };
    row("Instances", format!("{} ({mode} mode)", spec.instances));
    for s in &status.snapshot {
        let mut parts = Vec::new();
        if let Some(p) = s["pid"].as_u64() {
            parts.push(format!("pid {p}"));
        }
        if let Some(u) = uptime(s, now) {
            parts.push(format!("up {u}"));
        }
        parts.push(format!("restarts {}", s["restarts"]));
        if let Some(c) = s["exit"]["code"].as_i64() {
            parts.push(format!("last exit {c}"));
        }
        if let Some(sig) = s["exit"]["signal"].as_i64() {
            parts.push(format!("last signal {sig}"));
        }
        if let Some(r) = s["error"]["reason"].as_str() {
            parts.push(format!("error: {r}"));
        }
        row(
            "",
            format!(
                "{} {}: {}",
                s["instance"],
                s["state"].as_str().unwrap_or("?"),
                parts.join(", ")
            ),
        );
    }

    row("Exec", spec.exec.display().to_string());
    match &spec.cluster {
        Some(c) => {
            row("Script", c.script.display().to_string());
            if !c.node_args.is_empty() {
                row("Node args", c.node_args.join(" "));
            }
            row("Args", args_text(&c.args));
        }
        None => row("Args", args_text(&spec.args)),
    }
    row("Cwd", spec.cwd.display().to_string());
    let env: Vec<String> = spec
        .env
        .iter()
        .map(|(k, v)| {
            if show_env {
                format!("{k}={v}")
            } else {
                k.clone()
            }
        })
        .collect();
    row(
        "Environment",
        if show_env {
            env.join(" ")
        } else {
            format!("{} (values hidden; --show-env to show)", env.join(", "))
        },
    );
    row("Instance var", spec.instance_var.clone());

    let p = &spec.policy;
    let policy = spec.instance_policy();
    let restart = if p.autorestart {
        let delay = match p.exp_backoff_initial_ms {
            Some(i) => format!(
                "exponential from {} (max {})",
                ms(i),
                ms(p.exp_backoff_max_ms)
            ),
            None => ms(p.restart_delay_ms),
        };
        format!(
            "on exit, delay {delay}, max_restarts {}, min_uptime {}",
            p.max_restarts,
            ms(p.min_uptime_ms)
        )
    } else {
        "no (autorestart: false)".into()
    };
    row("Restart", restart);
    row(
        "Stop",
        format!(
            "{}, then SIGKILL after {}{}",
            p.kill_signal.name(),
            ms(p.kill_timeout_ms),
            if spec.treekill {
                ", whole process group"
            } else {
                ", main process only"
            }
        ),
    );
    if let Some(limit) = p.max_memory_bytes {
        row(
            "Memory",
            format!(
                "restart above {} resident (checked every {})",
                bytes(limit),
                ms(p.memory_check_interval_ms)
            ),
        );
    }
    row(
        "Readiness",
        match p.ready_timeout_ms {
            Some(t) => format!("waits for process.send('ready'), up to {}", ms(t)),
            None => "online as soon as it starts".into(),
        },
    );
    let strategy = match p.reload_strategy {
        ReloadStrategy::Auto => "auto",
        ReloadStrategy::StartFirst => "start_first",
        ReloadStrategy::StopFirst => "stop_first",
    };
    row(
        "Reload",
        match policy.reload {
            Some(ReplacementReady::Signal(t)) => format!(
                "{strategy}: new process first, healthy on {} within {}",
                if spec.cluster.is_some() && p.ready_timeout_ms.is_none() {
                    "listening"
                } else {
                    "ready"
                },
                human_duration(u64::try_from(t.as_millis()).unwrap_or(0))
            ),
            Some(ReplacementReady::Stable(t)) => format!(
                "{strategy}: new process first, healthy after {} up",
                human_duration(u64::try_from(t.as_millis()).unwrap_or(0))
            ),
            None => format!("{strategy}: restart (stop, then start)"),
        },
    );
    let l = &spec.logs;
    let rotation = if l.max_size_bytes == 0 {
        "no rotation".to_owned()
    } else {
        format!(
            "rotate at {} KiB, keep {}",
            l.max_size_bytes / 1024,
            l.retain
        )
    };
    row("Out log", l.out.display().to_string());
    row("Error log", l.err.display().to_string());
    row(
        "Logging",
        format!(
            "{rotation}, on overflow: {}",
            serde_json::to_value(l.overflow)
                .ok()
                .and_then(|v| v.as_str().map(str::to_owned))
                .unwrap_or_default()
        ),
    );

    if !events.is_empty() {
        println!();
        println!("Recent events:");
        crate::print_events(events);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations() {
        assert_eq!(human_duration(12_000), "12s");
        assert_eq!(human_duration(302_000), "5m 2s");
        assert_eq!(human_duration(3_600_000 * 4 + 60_000 * 3), "4h 3m");
        assert_eq!(human_duration(86_400_000 * 3 + 3_600_000), "3d 1h");
    }

    #[test]
    fn env_values_are_redacted_by_default() {
        let spec: AppSpec = serde_json::from_value(serde_json::json!({
            "name": "a", "exec": "/bin/true", "cwd": "/",
            "env": { "TOKEN": "secret" },
            "logs": { "out": "/o", "err": "/e" },
        }))
        .expect("spec");
        assert_eq!(spec_json(&spec, false)["env"]["TOKEN"], "<redacted>");
        assert_eq!(spec_json(&spec, true)["env"]["TOKEN"], "secret");
    }
}

/// Bytes in the largest whole binary unit: 200 MiB, 1.5 GiB.
pub fn bytes(b: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    #[allow(clippy::cast_precision_loss)]
    let mut v = b as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 || (v.fract()).abs() < 0.05 {
        format!("{v:.0} {}", UNITS[u])
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

#[cfg(test)]
mod bytes_tests {
    #[test]
    fn sizes_read_naturally() {
        assert_eq!(super::bytes(512), "512 B");
        assert_eq!(super::bytes(200 << 20), "200 MiB");
        assert_eq!(super::bytes(3 << 29), "1.5 GiB");
    }
}

//! Prometheus endpoint (docs/plan.md 6b.7): `GET /metrics` in the text
//! exposition format, for an existing Prometheus/Grafana setup.
//!
//! Off by default. `$VEXOR_HOME/settings.json`:
//! `{"prometheus": {"listen": "127.0.0.1:9615"}}`, re-read on change.
//! Any address other than loopback is allowed but warned about on every
//! command (docs/security.md T11): the endpoint has no authentication.

use std::fmt::Write as _;
use std::net::SocketAddr;

use serde::Deserialize;
use serde_json::Value;
use vexor_proto::InstanceMetrics;

pub const SETTINGS: &str = "settings.json";

#[derive(Debug, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    #[serde(default)]
    pub prometheus: Option<Prometheus>,
}

#[derive(Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Prometheus {
    pub listen: String,
}

impl Settings {
    pub fn parse(text: &str) -> Result<Settings, String> {
        let s: Settings = serde_json::from_str(text).map_err(|e| e.to_string())?;
        if let Some(p) = &s.prometheus {
            p.addr()?;
        }
        Ok(s)
    }
}

impl Prometheus {
    pub fn addr(&self) -> Result<SocketAddr, String> {
        self.listen.parse().map_err(|_| {
            format!(
                "prometheus.listen: {:?} is not an address like 127.0.0.1:9615",
                self.listen
            )
        })
    }
}

/// What the endpoint shows about one app.
pub struct AppView {
    pub name: String,
    pub running: bool,
    pub snapshot: Vec<Value>,
    pub metrics: Vec<InstanceMetrics>,
    /// (method, route, requests, 5xx responses), from JSON request logs.
    pub routes: Vec<(String, String, u64, u64)>,
}

/// A label value, escaped as the exposition format requires.
fn label(v: &str) -> String {
    v.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

struct Family<'a> {
    out: &'a mut String,
}

impl Family<'_> {
    fn header(&mut self, name: &str, kind: &str, help: &str) {
        let _ = writeln!(self.out, "# HELP {name} {help}\n# TYPE {name} {kind}");
    }

    fn sample(&mut self, name: &str, labels: &[(&str, &str)], value: f64) {
        let labels: Vec<String> = labels
            .iter()
            .map(|(k, v)| format!("{k}=\"{}\"", label(v)))
            .collect();
        let _ = writeln!(self.out, "{name}{{{}}} {value}", labels.join(","));
    }
}

/// Render every app's instances, and the open alert incidents per rule
/// and level.
pub fn render(apps: &[AppView], open_alerts: &[(String, String, String)], now_ms: u64) -> String {
    let mut out = String::new();
    let mut f = Family { out: &mut out };
    f.header("vexor_build_info", "gauge", "Vexor version.");
    f.sample(
        "vexor_build_info",
        &[("version", env!("CARGO_PKG_VERSION"))],
        1.0,
    );

    // (app, instance, running state, snapshot, latest sample)
    struct Row<'a> {
        app: &'a str,
        instance: String,
        running: bool,
        snapshot: &'a Value,
        sample: Option<&'a InstanceMetrics>,
    }
    let mut rows = Vec::new();
    for a in apps {
        for s in &a.snapshot {
            let Some(i) = s["instance"].as_u64() else {
                continue;
            };
            rows.push(Row {
                app: &a.name,
                instance: i.to_string(),
                running: a.running,
                snapshot: s,
                sample: a.metrics.iter().find(|m| {
                    u64::from(m.instance) == i && s["pid"].as_u64() == Some(u64::from(m.pid))
                }),
            });
        }
    }
    let online = |r: &Row<'_>| r.running && r.snapshot["state"] == "online";
    let gauge = |f: &mut Family<'_>,
                 name: &str,
                 kind: &str,
                 help: &str,
                 value: &dyn Fn(&Row<'_>) -> Option<f64>| {
        f.header(name, kind, help);
        for r in &rows {
            if let Some(v) = value(r) {
                f.sample(name, &[("app", r.app), ("instance", &r.instance)], v);
            }
        }
    };
    #[allow(clippy::cast_precision_loss)]
    {
        gauge(
            &mut f,
            "vexor_instance_up",
            "gauge",
            "1 when the instance is online.",
            &|r| Some(if online(r) { 1.0 } else { 0.0 }),
        );
        gauge(
            &mut f,
            "vexor_instance_restarts_total",
            "counter",
            "Restarts since the app's supervisor started.",
            &|r| r.snapshot["restarts"].as_u64().map(|n| n as f64),
        );
        gauge(
            &mut f,
            "vexor_instance_uptime_seconds",
            "gauge",
            "Seconds since the instance came online.",
            &|r| {
                online(r)
                    .then(|| r.snapshot["ts_ms"].as_u64())
                    .flatten()
                    .map(|t| now_ms.saturating_sub(t) as f64 / 1000.0)
            },
        );
        gauge(
            &mut f,
            "vexor_instance_cpu_percent",
            "gauge",
            "CPU over the last sampling interval, % of one core.",
            &|r| r.sample.and_then(|m| m.cpu_pct),
        );
        gauge(
            &mut f,
            "vexor_instance_memory_bytes",
            "gauge",
            "Resident memory.",
            &|r| r.sample.map(|m| m.rss_bytes as f64),
        );
        gauge(
            &mut f,
            "vexor_instance_open_files",
            "gauge",
            "Open file descriptors.",
            &|r| r.sample.and_then(|m| m.open_fds).map(|n| n as f64),
        );
        gauge(
            &mut f,
            "vexor_instance_health_ok",
            "gauge",
            "1 when the latest health check passed, 0 when it failed.",
            &|r| {
                r.sample
                    .and_then(|m| m.health.as_ref())
                    .map(|h| if h.ok { 1.0 } else { 0.0 })
            },
        );
    }
    f.header(
        "vexor_instance_state",
        "gauge",
        "1 for the instance's current state.",
    );
    for r in &rows {
        let state = if r.running {
            r.snapshot["state"].as_str().unwrap_or("unknown")
        } else {
            "stopped"
        };
        f.sample(
            "vexor_instance_state",
            &[("app", r.app), ("instance", &r.instance), ("state", state)],
            1.0,
        );
    }

    type Route = (String, String, u64, u64);
    #[allow(clippy::cast_precision_loss)]
    let counter = |f: &mut Family<'_>, name: &str, help: &str, value: fn(&Route) -> u64| {
        f.header(name, "counter", help);
        for a in apps {
            for t in &a.routes {
                f.sample(
                    name,
                    &[("app", &a.name), ("method", &t.0), ("route", &t.1)],
                    value(t) as f64,
                );
            }
        }
    };
    counter(
        &mut f,
        "vexor_route_requests_total",
        "Requests per route, read from the app's JSON request logs.",
        |t| t.2,
    );
    counter(
        &mut f,
        "vexor_route_server_errors_total",
        "Responses with a 5xx status per route, from JSON request logs.",
        |t| t.3,
    );

    f.header(
        "vexor_alerts_open",
        "gauge",
        "Open alert incidents, by rule and level.",
    );
    let mut counts: std::collections::BTreeMap<(&str, &str), u32> =
        std::collections::BTreeMap::new();
    for (_, rule, level) in open_alerts {
        *counts.entry((rule.as_str(), level.as_str())).or_default() += 1;
    }
    for ((rule, level), n) in counts {
        f.sample(
            "vexor_alerts_open",
            &[("rule", rule), ("level", level)],
            f64::from(n),
        );
    }
    out
}

/// Whether `addr` is reachable only from this machine.
pub fn is_loopback(addr: &SocketAddr) -> bool {
    addr.ip().is_loopback()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn renders_instances_and_alerts() {
        let apps = [AppView {
            name: "we\"b".into(),
            running: true,
            snapshot: vec![
                json!({ "instance": 0, "state": "online", "pid": 10, "restarts": 2, "ts_ms": 1_000 }),
                json!({ "instance": 1, "state": "backoff", "restarts": 5 }),
            ],
            metrics: vec![InstanceMetrics {
                instance: 0,
                pid: 10,
                ts_ms: 9_000,
                cpu_pct: Some(12.5),
                rss_bytes: 1024,
                open_fds: Some(7),
                health: None,
            }],
            routes: vec![("GET".into(), "/users/:id".into(), 40, 3)],
        }];
        let alerts = [
            ("web".to_owned(), "memory".to_owned(), "warning".to_owned()),
            ("api".to_owned(), "memory".to_owned(), "warning".to_owned()),
        ];
        let text = render(&apps, &alerts, 11_000);
        for line in [
            r#"vexor_instance_up{app="we\"b",instance="0"} 1"#,
            r#"vexor_instance_up{app="we\"b",instance="1"} 0"#,
            r#"vexor_instance_state{app="we\"b",instance="1",state="backoff"} 1"#,
            r#"vexor_instance_restarts_total{app="we\"b",instance="1"} 5"#,
            r#"vexor_instance_uptime_seconds{app="we\"b",instance="0"} 10"#,
            r#"vexor_instance_cpu_percent{app="we\"b",instance="0"} 12.5"#,
            r#"vexor_instance_memory_bytes{app="we\"b",instance="0"} 1024"#,
            r#"vexor_instance_open_files{app="we\"b",instance="0"} 7"#,
            r#"vexor_alerts_open{rule="memory",level="warning"} 2"#,
            r#"vexor_route_requests_total{app="we\"b",method="GET",route="/users/:id"} 40"#,
            r#"vexor_route_server_errors_total{app="we\"b",method="GET",route="/users/:id"} 3"#,
            "# TYPE vexor_instance_restarts_total counter",
        ] {
            assert!(text.lines().any(|l| l == line), "missing {line}\n{text}");
        }
        assert!(!text.contains(r#"vexor_instance_cpu_percent{app="we\"b",instance="1"}"#));
    }

    #[test]
    fn settings_validation() {
        assert_eq!(Settings::parse("{}").expect("empty"), Settings::default());
        let s = Settings::parse(r#"{"prometheus": {"listen": "127.0.0.1:9615"}}"#).expect("valid");
        assert!(is_loopback(
            &s.prometheus.expect("prom").addr().expect("addr")
        ));
        assert!(!is_loopback(&"0.0.0.0:9615".parse().expect("addr")));
        for bad in [
            r#"{"prometheus": {"listen": "localhost"}}"#,
            r#"{"prometheus": {"port": 1}}"#,
            r#"{"metrics": {}}"#,
        ] {
            assert!(Settings::parse(bad).is_err(), "{bad}");
        }
    }
}

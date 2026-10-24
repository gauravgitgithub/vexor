//! Alert rules (docs/plan.md 6b.3): thresholds on what the daemon already
//! knows about each app (resource samples, states, restarts, health, log
//! sizes), with `warning` and `danger` levels that must hold for `for`
//! before an incident opens or escalates, and stay clear as long before it
//! recovers.
//!
//! One incident per (rule, app, instance): it notifies when it opens, when
//! it escalates to danger and when it recovers, never twice for the same
//! level. An incident that opens 3 times within an hour is flapping: one
//! `flapping` notice, then silence until it has stayed clear for the
//! cooldown, then `recovered`. Every transition is recorded either way.
//!
//! Configured in `$VEXOR_HOME/alerts.json`; without it the built-in rules
//! below apply. Pure logic: the daemon feeds observations and a clock.

use std::collections::{BTreeMap, VecDeque};

use serde::{Deserialize, Serialize};

pub const FILE: &str = "alerts.json";

/// Incidents opened this many times within `FLAP_WINDOW_MS` are flapping.
const FLAP_OPENS: usize = 3;
const FLAP_WINDOW_MS: u64 = 3_600_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Level {
    Ok,
    Warning,
    Danger,
}

/// What a rule measures.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Metric {
    /// An instance gave up restarting (errored): 1, else 0.
    CrashLoop,
    /// Restarts of an instance in the last hour.
    Restarts,
    /// Health check failing, or the instance is unhealthy: 1, else 0.
    Unhealthy,
    /// CPU, % of one core.
    Cpu,
    /// Resident memory, bytes.
    Memory,
    OpenFiles,
    /// An app's log files on disk, rotated ones included, bytes.
    LogDisk,
}

impl Metric {
    pub const ALL: [Metric; 7] = [
        Metric::CrashLoop,
        Metric::Restarts,
        Metric::Unhealthy,
        Metric::Cpu,
        Metric::Memory,
        Metric::OpenFiles,
        Metric::LogDisk,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Metric::CrashLoop => "crash_loop",
            Metric::Restarts => "restarts",
            Metric::Unhealthy => "unhealthy",
            Metric::Cpu => "cpu",
            Metric::Memory => "memory",
            Metric::OpenFiles => "open_files",
            Metric::LogDisk => "log_disk",
        }
    }

    /// Built-in thresholds: problems that need someone whatever the app.
    /// CPU, memory and open files depend on the app, so they are off
    /// until configured.
    fn default_rule(self) -> Rule {
        let rule = |warning, danger, for_ms| Rule {
            enabled: true,
            warning,
            danger,
            for_ms,
            apps: Vec::new(),
        };
        match self {
            Metric::CrashLoop => rule(None, Some(1.0), 0),
            Metric::Restarts => rule(Some(3.0), Some(10.0), 0),
            Metric::Unhealthy => rule(None, Some(1.0), 30_000),
            Metric::LogDisk => rule(
                Some(1024.0 * 1024.0 * 1024.0),
                Some(5.0 * 1024.0 * 1024.0 * 1024.0),
                0,
            ),
            Metric::Cpu | Metric::Memory | Metric::OpenFiles => Rule {
                enabled: false,
                ..rule(None, None, 60_000)
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Rule {
    pub enabled: bool,
    pub warning: Option<f64>,
    pub danger: Option<f64>,
    /// How long a level must hold before it counts (and clear before an
    /// incident recovers).
    pub for_ms: u64,
    /// Apps it applies to; empty means every app.
    pub apps: Vec<String>,
}

impl Rule {
    fn applies_to(&self, app: &str) -> bool {
        self.enabled && (self.apps.is_empty() || self.apps.iter().any(|a| a == app))
    }

    fn level(&self, value: f64) -> Level {
        if self.danger.is_some_and(|d| value >= d) {
            Level::Danger
        } else if self.warning.is_some_and(|w| value >= w) {
            Level::Warning
        } else {
            Level::Ok
        }
    }

    pub fn threshold(&self, level: Level) -> Option<f64> {
        match level {
            Level::Danger => self.danger,
            Level::Warning => self.warning,
            Level::Ok => self.warning.or(self.danger),
        }
    }
}

/// `alerts.json`, as written by people.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfigFile {
    #[serde(default)]
    cooldown: Option<String>,
    #[serde(default)]
    rules: BTreeMap<Metric, RuleFile>,
    #[serde(default)]
    channels: crate::notify::ChannelsFile,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RuleFile {
    enabled: Option<bool>,
    warning: Option<Threshold>,
    danger: Option<Threshold>,
    #[serde(rename = "for")]
    for_: Option<String>,
    #[serde(default)]
    apps: Vec<String>,
}

/// A number, or a size like "512M" / a percentage like "80%".
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum Threshold {
    Number(f64),
    Text(String),
}

impl Threshold {
    fn value(&self) -> Result<f64, String> {
        match self {
            Threshold::Number(n) => Ok(*n),
            Threshold::Text(t) => {
                let t = t.trim();
                if let Some(p) = t.strip_suffix('%') {
                    return p
                        .trim()
                        .parse()
                        .map_err(|_| format!("invalid percentage {t:?}"));
                }
                parse_size(t).map(|b| b as f64)
            }
        }
    }
}

/// "512", "64K", "512M", "1G" (powers of 1024).
fn parse_size(s: &str) -> Result<u64, String> {
    let (num, mult) = match s.chars().last().map(|c| c.to_ascii_uppercase()) {
        Some('K') => (&s[..s.len() - 1], 1u64 << 10),
        Some('M') => (&s[..s.len() - 1], 1 << 20),
        Some('G') => (&s[..s.len() - 1], 1 << 30),
        _ => (s, 1),
    };
    let n: f64 = num
        .trim()
        .parse()
        .map_err(|_| format!("invalid size {s:?}"))?;
    if n.is_nan() || n < 0.0 {
        return Err(format!("invalid size {s:?}"));
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    Ok((n * mult as f64) as u64)
}

/// "30s", "5m", "1h", "500ms", or a number of milliseconds.
fn parse_duration_ms(s: &str) -> Result<u64, String> {
    let s = s.trim();
    let (num, mult) = if let Some(n) = s.strip_suffix("ms") {
        (n, 1)
    } else if let Some(n) = s.strip_suffix('s') {
        (n, 1000)
    } else if let Some(n) = s.strip_suffix('m') {
        (n, 60_000)
    } else if let Some(n) = s.strip_suffix('h') {
        (n, 3_600_000)
    } else {
        (s, 1)
    };
    num.trim()
        .parse::<u64>()
        .map(|n| n * mult)
        .map_err(|_| format!("invalid duration {s:?}"))
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Config {
    pub cooldown_ms: u64,
    pub rules: BTreeMap<Metric, Rule>,
    /// Where notifications go; secrets are redacted when serialized.
    pub channels: crate::notify::Channels,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            cooldown_ms: 600_000,
            rules: Metric::ALL.iter().map(|m| (*m, m.default_rule())).collect(),
            channels: crate::notify::Channels::default(),
        }
    }
}

impl Config {
    /// Parse `alerts.json`: each rule given overrides the built-in one for
    /// its metric, field by field.
    pub fn parse(text: &str) -> Result<Config, String> {
        let file: ConfigFile = serde_json::from_str(text).map_err(|e| e.to_string())?;
        let mut config = Config::default();
        if let Some(c) = &file.cooldown {
            config.cooldown_ms = parse_duration_ms(c)?;
        }
        config.channels = file.channels.resolve()?;
        for (metric, r) in file.rules {
            let rule = config
                .rules
                .get_mut(&metric)
                .expect("every metric has a default");
            let name = metric.name();
            let given = r.warning.is_some() || r.danger.is_some();
            if let Some(w) = &r.warning {
                rule.warning = Some(w.value().map_err(|e| format!("{name}.warning: {e}"))?);
            }
            if let Some(d) = &r.danger {
                rule.danger = Some(d.value().map_err(|e| format!("{name}.danger: {e}"))?);
            }
            if let Some(f) = &r.for_ {
                rule.for_ms = parse_duration_ms(f).map_err(|e| format!("{name}.for: {e}"))?;
            }
            rule.apps = r.apps;
            // Giving thresholds turns an off-by-default rule on.
            rule.enabled = r.enabled.unwrap_or(rule.enabled || given);
            if let (Some(w), Some(d)) = (rule.warning, rule.danger)
                && w > d
            {
                return Err(format!("{name}: warning ({w}) is above danger ({d})"));
            }
            if rule.enabled && rule.warning.is_none() && rule.danger.is_none() {
                return Err(format!(
                    "{name}: enabled without a warning or danger threshold"
                ));
            }
        }
        Ok(config)
    }
}

/// A value in its rule's unit, for notifications: bytes, % of a core,
/// restarts an hour, or yes/no for the on/off checks.
pub fn value_text(metric: Metric, v: f64) -> String {
    match metric {
        Metric::Memory | Metric::LogDisk => {
            let units = ["B", "KiB", "MiB", "GiB", "TiB"];
            let mut x = v;
            let mut i = 0;
            while x >= 1024.0 && i < units.len() - 1 {
                x /= 1024.0;
                i += 1;
            }
            if i == 0 {
                format!("{x:.0} B")
            } else {
                format!("{x:.1} {}", units[i])
            }
        }
        Metric::Cpu => format!("{v:.1}%"),
        Metric::Restarts => format!("{v}/h"),
        Metric::CrashLoop | Metric::Unhealthy => if v >= 1.0 { "yes" } else { "no" }.into(),
        Metric::OpenFiles => format!("{v}"),
    }
}

/// What an incident is about.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub struct Key {
    pub metric: Metric,
    pub app: String,
    /// `None` for app-wide measurements (log disk).
    pub instance: Option<u32>,
}

/// A recorded change of an incident.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Change {
    pub key: Key,
    /// `warning`, `danger`, `recovered` or `flapping`.
    pub kind: &'static str,
    pub value: f64,
    pub threshold: Option<f64>,
    /// False while flapping: recorded, but nobody is notified.
    pub notify: bool,
}

/// An open incident, for `vexor alerts`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Open {
    pub key: Key,
    pub level: Level,
    pub since_ms: u64,
    pub value: f64,
    pub flapping: bool,
}

#[derive(Debug, Default)]
struct Track {
    level: Option<Level>,
    opened_ms: u64,
    last_value: f64,
    above_warning_since: Option<u64>,
    above_danger_since: Option<u64>,
    clear_since: Option<u64>,
    opens: VecDeque<u64>,
    flapping: bool,
    seen_ms: u64,
}

#[derive(Debug, Default)]
pub struct Engine {
    tracks: BTreeMap<Key, Track>,
}

impl Engine {
    /// Feed one measurement taken at `now`. Returns what changed, if anything.
    pub fn observe(&mut self, config: &Config, key: Key, value: f64, now: u64) -> Option<Change> {
        let rule = config.rules.get(&key.metric)?;
        if !rule.applies_to(&key.app) {
            self.tracks.remove(&key);
            return None;
        }
        let t = self.tracks.entry(key.clone()).or_default();
        t.seen_ms = now;
        t.last_value = value;
        let target = rule.level(value);
        let since = |slot: &mut Option<u64>, on: bool| {
            if on {
                *slot.get_or_insert(now)
            } else {
                *slot = None;
                u64::MAX
            }
        };
        let held = |start: u64| start != u64::MAX && now.saturating_sub(start) >= rule.for_ms;
        let danger_start = since(&mut t.above_danger_since, target == Level::Danger);
        let warning_start = since(&mut t.above_warning_since, target >= Level::Warning);
        let clear_start = since(&mut t.clear_since, target == Level::Ok);
        let level = t.level.unwrap_or(Level::Ok);

        let rise = if level < Level::Danger && held(danger_start) {
            Some(Level::Danger)
        } else if level < Level::Warning && held(warning_start) {
            Some(Level::Warning)
        } else {
            None
        };
        if let Some(new) = rise {
            let opening = level == Level::Ok;
            t.level = Some(new);
            let mut kind = if new == Level::Danger {
                "danger"
            } else {
                "warning"
            };
            if opening {
                t.opened_ms = now;
                t.opens.push_back(now);
                while t
                    .opens
                    .front()
                    .is_some_and(|&o| now.saturating_sub(o) > FLAP_WINDOW_MS)
                {
                    t.opens.pop_front();
                }
                if !t.flapping && t.opens.len() >= FLAP_OPENS {
                    t.flapping = true;
                    kind = "flapping";
                    return Some(Change {
                        key,
                        kind,
                        value,
                        threshold: rule.threshold(new),
                        notify: true,
                    });
                }
            }
            return Some(Change {
                key,
                kind,
                value,
                threshold: rule.threshold(new),
                notify: !t.flapping,
            });
        }
        if level > Level::Ok && held(clear_start) {
            t.level = Some(Level::Ok);
            return Some(Change {
                key,
                kind: "recovered",
                value,
                threshold: rule.threshold(Level::Ok),
                notify: !t.flapping,
            });
        }
        // Flapping ends once clear for the cooldown: one recovered notice.
        if t.flapping
            && level == Level::Ok
            && clear_start != u64::MAX
            && now.saturating_sub(clear_start) >= config.cooldown_ms.max(rule.for_ms)
        {
            t.flapping = false;
            t.opens.clear();
            return Some(Change {
                key,
                kind: "recovered",
                value,
                threshold: rule.threshold(Level::Ok),
                notify: true,
            });
        }
        None
    }

    /// Forget measurements not seen since `before` (an app deleted, an
    /// instance count lowered). Their open incidents end without a notice.
    pub fn forget_older_than(&mut self, before: u64) {
        self.tracks.retain(|_, t| t.seen_ms >= before);
    }

    pub fn open(&self) -> Vec<Open> {
        self.tracks
            .iter()
            .filter_map(|(k, t)| {
                let level = t.level.filter(|l| *l > Level::Ok)?;
                Some(Open {
                    key: k.clone(),
                    level,
                    since_ms: t.opened_ms,
                    value: t.last_value,
                    flapping: t.flapping,
                })
            })
            .collect()
    }
}

/// Restarts of each instance within the last hour, from its restart
/// counter as seen over time (the counter restarts at 0 with a new shim).
#[derive(Debug, Default)]
pub struct RestartRate {
    seen: BTreeMap<(String, u32), VecDeque<(u64, u64)>>,
}

impl RestartRate {
    pub fn observe(&mut self, app: &str, instance: u32, restarts: u64, now: u64) -> u64 {
        let q = self.seen.entry((app.to_owned(), instance)).or_default();
        if q.back().is_some_and(|&(_, r)| restarts < r) {
            q.clear();
        }
        q.push_back((now, restarts));
        // Keep the last reading from before the hour as the baseline.
        while q
            .get(1)
            .is_some_and(|&(t, _)| now.saturating_sub(t) >= 3_600_000)
        {
            q.pop_front();
        }
        restarts - q.front().map_or(restarts, |&(_, r)| r)
    }

    pub fn forget_older_than(&mut self, before: u64) {
        self.seen
            .retain(|_, q| q.back().is_some_and(|&(t, _)| t >= before));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(metric: Metric) -> Key {
        Key {
            metric,
            app: "web".into(),
            instance: Some(0),
        }
    }

    fn mem_config(for_ms: u64) -> Config {
        Config::parse(&format!(
            r#"{{"cooldown": "10m", "rules": {{"memory": {{"warning": "100M", "danger": "200M", "for": "{for_ms}ms"}}}}}}"#
        ))
        .expect("config")
    }

    const M: f64 = 1024.0 * 1024.0;

    /// Feed `values` one per second from `start`, collecting change kinds.
    fn run(
        e: &mut Engine,
        c: &Config,
        start: u64,
        values: &[f64],
    ) -> Vec<(u64, &'static str, bool)> {
        values
            .iter()
            .enumerate()
            .filter_map(|(i, v)| {
                let now = start + i as u64 * 1000;
                e.observe(c, key(Metric::Memory), *v, now)
                    .map(|ch| (now, ch.kind, ch.notify))
            })
            .collect()
    }

    #[test]
    fn opens_after_for_escalates_and_recovers_once_each() {
        let c = mem_config(3_000);
        let mut e = Engine::default();
        let values = [
            50.0, 150.0, 150.0, 150.0, 150.0, 150.0, // warning held 3 s at t=4
            250.0, 250.0, 250.0, 250.0, 250.0, // danger held 3 s at t=9
            150.0, 50.0, 50.0, 50.0, 50.0, 50.0, // clear held 3 s at t=15
        ]
        .map(|v| v * M);
        assert_eq!(
            run(&mut e, &c, 0, &values),
            [
                (4_000, "warning", true),
                (9_000, "danger", true),
                (15_000, "recovered", true)
            ]
        );
        assert!(e.open().is_empty());
    }

    #[test]
    fn a_short_spike_does_not_open_and_a_ramp_counts_from_the_warning_level() {
        let c = mem_config(3_000);
        let mut e = Engine::default();
        // 2 s above warning, then back: nothing.
        assert!(run(&mut e, &c, 0, &[150.0 * M, 150.0 * M, 50.0 * M, 50.0 * M]).is_empty());
        // Warning for 2 s then danger: warning has held 3 s at the third
        // sample even though the level changed.
        assert_eq!(
            run(
                &mut e,
                &c,
                10_000,
                &[150.0 * M, 150.0 * M, 250.0 * M, 250.0 * M]
            ),
            [(13_000, "warning", true)]
        );
    }

    #[test]
    fn three_openings_in_an_hour_flap_quietly_then_recover_after_cooldown() {
        let c = mem_config(0);
        let mut e = Engine::default();
        let mut kinds = Vec::new();
        let mut t = 0;
        for _ in 0..4 {
            kinds.extend(run(&mut e, &c, t, &[150.0 * M, 50.0 * M]));
            t += 60_000;
        }
        assert_eq!(
            kinds.iter().map(|k| (k.1, k.2)).collect::<Vec<_>>(),
            [
                ("warning", true),
                ("recovered", true),
                ("warning", true),
                ("recovered", true),
                ("flapping", true),
                ("recovered", false),
                ("warning", false),
                ("recovered", false),
            ]
        );
        assert!(e.open().is_empty());
        // Clear for the 10 minute cooldown: one recovered notice.
        let quiet = run(&mut e, &c, t, &[50.0 * M]);
        assert!(quiet.is_empty());
        let after = e.observe(&c, key(Metric::Memory), 50.0 * M, t + 600_000);
        assert_eq!(after.map(|a| (a.kind, a.notify)), Some(("recovered", true)));
    }

    #[test]
    fn defaults_cover_crash_loops_restarts_health_and_log_disk() {
        let c = Config::default();
        let mut e = Engine::default();
        let crash = e
            .observe(&c, key(Metric::CrashLoop), 1.0, 0)
            .expect("immediate");
        assert_eq!((crash.kind, crash.threshold), ("danger", Some(1.0)));
        assert!(e.observe(&c, key(Metric::Unhealthy), 1.0, 0).is_none());
        assert_eq!(
            e.observe(&c, key(Metric::Unhealthy), 1.0, 30_000)
                .map(|c| c.kind),
            Some("danger")
        );
        assert_eq!(
            e.observe(&c, key(Metric::Restarts), 3.0, 0).map(|c| c.kind),
            Some("warning")
        );
        assert!(
            e.observe(&c, key(Metric::Memory), 1e12, 0).is_none()
                && e.observe(&c, key(Metric::Memory), 1e12, 3_600_000)
                    .is_none(),
            "memory is off until configured"
        );
        assert_eq!(e.open().len(), 3);
    }

    #[test]
    fn config_parsing_and_validation() {
        let c = Config::parse(
            r#"{"rules": {"cpu": {"warning": "80%", "for": "5m", "apps": ["api"]},
                          "restarts": {"enabled": false}}}"#,
        )
        .expect("valid");
        let cpu = &c.rules[&Metric::Cpu];
        assert!(cpu.enabled && cpu.applies_to("api") && !cpu.applies_to("web"));
        assert_eq!((cpu.warning, cpu.for_ms), (Some(80.0), 300_000));
        assert!(!c.rules[&Metric::Restarts].enabled);
        assert_eq!(c.rules[&Metric::LogDisk].warning, Some(1024.0 * M));
        for bad in [
            r#"{"rules": {"memory": {"warning": "2G", "danger": "1G"}}}"#,
            r#"{"rules": {"memory": {"enabled": true}}}"#,
            r#"{"rules": {"disk": {"warning": 1}}}"#,
            r#"{"rules": {"cpu": {"warning": "lots"}}}"#,
            r#"{"cooldown": "soon"}"#,
            r#"{"rule": {}}"#,
        ] {
            assert!(Config::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn restart_rate_counts_the_last_hour_and_survives_counter_resets() {
        let mut r = RestartRate::default();
        assert_eq!(r.observe("web", 0, 5, 0), 0, "history starts now");
        assert_eq!(r.observe("web", 0, 7, 1_000), 2);
        assert_eq!(r.observe("web", 0, 9, 3_600_000), 4);
        assert_eq!(
            r.observe("web", 0, 9, 3_602_000),
            2,
            "the first two aged out"
        );
        assert_eq!(
            r.observe("web", 0, 1, 3_603_000),
            0,
            "new shim: counter reset"
        );
        assert_eq!(r.observe("web", 0, 3, 3_604_000), 2);
    }
}

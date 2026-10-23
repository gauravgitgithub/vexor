//! `cron_restart`: restart every instance on a schedule, as PM2 does.
//!
//! Patterns follow PM2 7, which uses the JavaScript `croner` library; this
//! is its Rust port, configured the same way: 5 fields, or 6 with seconds
//! first; day-of-month and day-of-week match when either does. Times are in
//! the machine's local time zone.

use std::time::Duration;

use chrono::Local;
use croner::Cron;
use croner::parser::{CronParser, Seconds, Year};
use tokio::sync::broadcast;

use crate::instance::Control;
use crate::output::{Output, ShimEvent};

pub fn parse(pattern: &str) -> Result<Cron, String> {
    CronParser::builder()
        .seconds(Seconds::Optional)
        .year(Year::Disallowed)
        .build()
        .parse(pattern)
        .map_err(|e| format!("invalid cron_restart {pattern:?}: {e}"))
}

/// Longest sleep before re-reading the clock, so a clock change (NTP, a
/// suspended machine) delays a restart by at most this much.
const RECHECK: Duration = Duration::from_secs(30);

/// Send a restart to every instance at each scheduled time. Runs until the
/// pattern has no further occurrence.
pub async fn schedule(cron: Cron, app: String, out: Output, restart: broadcast::Sender<Control>) {
    loop {
        let next = match cron.find_next_occurrence(&Local::now(), false) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("vexor shim: cron_restart has no next run: {e}");
                return;
            }
        };
        loop {
            let left = (next - Local::now()).to_std().unwrap_or_default();
            if left.is_zero() {
                break;
            }
            tokio::time::sleep(left.min(RECHECK)).await;
        }
        out.emit(&ShimEvent::CronRestart {
            app: &app,
            scheduled: next.to_rfc3339(),
        });
        let _ = restart.send(Control::Restart);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pm2_patterns_parse() {
        for p in [
            "0 3 * * *",
            "*/5 * * * *",
            "0 0 * * 1-5",
            "*/2 * * * * *",
            "@daily",
            "0 12 1 * SUN",
        ] {
            assert!(parse(p).is_ok(), "{p}");
        }
        for p in ["", "61 * * * *", "* * *", "0 0 0 1 1 * 2030", "every day"] {
            assert!(parse(p).is_err(), "{p}");
        }
    }

    #[test]
    fn next_occurrence_is_in_the_future() {
        let c = parse("*/2 * * * * *").expect("parse");
        let now = Local::now();
        let next = c.find_next_occurrence(&now, false).expect("next");
        assert!(next > now && next - now <= chrono::TimeDelta::seconds(2));
    }
}

//! `vexor monit`: a live view of every app (PM2 `monit`): one row per
//! instance with CPU and memory bars, restarts, uptime and health, the
//! open alerts, and the latest log lines of all apps. Redrawn every
//! second until Ctrl-C; prints a single frame when not on a terminal.

use std::io::{IsTerminal, Write};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use serde_json::Value;
use vexor_core::AppSpec;
use vexor_proto::{AppStatus, Request, Response};

use crate::describe::{bytes, now_ms, uptime};
use crate::{client, code, describe_app, logs, ui};

const LOG_LINES: usize = 8;
/// Specs (log paths) change rarely: re-read this often.
const SPEC_REFRESH: Duration = Duration::from_secs(10);

/// A bar of `width` cells for `fraction` (0..=1).
pub fn bar(fraction: f64, width: usize) -> String {
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss
    )]
    let filled = ((fraction.clamp(0.0, 1.0) * width as f64).round() as usize).min(width);
    format!("{}{}", "█".repeat(filled), "·".repeat(width - filled))
}

fn frame(apps: &[AppStatus], alerts: &[Value], log_lines: &[String]) -> String {
    let now = now_ms();
    let mut out = String::new();
    let max_rss = apps
        .iter()
        .flat_map(|a| a.metrics.iter().map(|m| m.rss_bytes))
        .max()
        .unwrap_or(1)
        .max(1);
    let mut t = ui::Table::new(&[
        "App", "#", "Status", "CPU", "", "Memory", "", "↺", "Uptime", "Health",
    ])
    .right(&[1, 3, 5, 7, 8]);
    for a in apps {
        for s in &a.snapshot {
            let i = s["instance"].as_u64().unwrap_or(0);
            let m = a.metrics.iter().find(|m| {
                u64::from(m.instance) == i && s["pid"].as_u64() == Some(u64::from(m.pid))
            });
            let state = s["state"].as_str().unwrap_or("?");
            let (cpu, cpu_bar) = m.and_then(|m| m.cpu_pct).map_or_else(
                || (ui::dim("-"), String::new()),
                |c| (format!("{c:.1}%"), ui::green(&bar(c / 100.0, 10))),
            );
            #[allow(clippy::cast_precision_loss)]
            let (mem, mem_bar) = m.map_or_else(
                || (ui::dim("-"), String::new()),
                |m| {
                    (
                        bytes(m.rss_bytes),
                        ui::magenta(&bar(m.rss_bytes as f64 / max_rss as f64, 10)),
                    )
                },
            );
            let health = match m.and_then(|m| m.health.as_ref()) {
                Some(h) if h.ok => ui::green("passing"),
                Some(_) => ui::bold_red("failing"),
                None => ui::dim("-"),
            };
            t.row(vec![
                ui::app(&a.name),
                i.to_string(),
                ui::state(state),
                cpu,
                cpu_bar,
                mem,
                mem_bar,
                s["restarts"].as_u64().unwrap_or(0).to_string(),
                uptime(s, now).unwrap_or_else(|| ui::dim("-")),
                health,
            ]);
        }
    }
    out.push_str(&t.render());
    out.push('\n');
    if alerts.is_empty() {
        out.push_str(&ui::dim("No open alerts.\n"));
    } else {
        out.push_str(&ui::bold("Open alerts\n"));
        for o in alerts {
            let rule = o["key"]["metric"].as_str().unwrap_or("?");
            out.push_str(&format!(
                "  {} {} #{} {rule} ({})\n",
                crate::alerts::level(o["level"].as_str().unwrap_or("?")),
                ui::app(o["key"]["app"].as_str().unwrap_or("?")),
                o["key"]["instance"]
                    .as_u64()
                    .map_or_else(|| "-".into(), |i| i.to_string()),
                crate::alerts::value(rule, o["value"].as_f64().unwrap_or(0.0))
            ));
        }
    }
    out.push('\n');
    out.push_str(&ui::bold("Latest logs\n"));
    let width = ui::term_width().unwrap_or(120);
    for l in log_lines {
        out.push_str(&ui::truncate(l, width));
        out.push('\n');
    }
    out
}

/// The latest lines of every app's logs, oldest first. Lines start with
/// their timestamp, so sorting them orders the apps' output by time.
fn latest_logs(specs: &[AppSpec]) -> Vec<String> {
    let mut lines = Vec::new();
    for s in specs {
        let mut paths = vec![(s.logs.out.clone(), "|")];
        if s.logs.err != s.logs.out {
            paths.push((s.logs.err.clone(), "!"));
        }
        for (p, mark) in paths {
            for l in logs::last_lines_with_rotated(&p, LOG_LINES) {
                lines.push((l.clone(), format!("{} {mark} {l}", ui::app(&s.name))));
            }
        }
    }
    lines.sort_by(|a, b| a.0.cmp(&b.0));
    let skip = lines.len().saturating_sub(LOG_LINES);
    lines
        .into_iter()
        .skip(skip)
        .map(|(_, shown)| shown)
        .collect()
}

pub fn run(home: &std::path::Path) -> ExitCode {
    let live = std::io::stdout().is_terminal();
    let mut specs: Vec<AppSpec> = Vec::new();
    let mut specs_read: Option<Instant> = None;
    loop {
        let apps = match client::request(home, &Request::List) {
            Ok(Response::Ok { apps, .. }) => apps,
            Ok(Response::Error { message, .. }) => return crate::fail(&message),
            Ok(_) => return crate::fail("unexpected answer from the daemon"),
            Err(e) => {
                eprintln!("vexor: {e}");
                return ExitCode::from(code::DAEMON_UNREACHABLE);
            }
        };
        ui::assign_app_colors(apps.iter().map(|a| a.name.as_str()));
        if specs_read.is_none_or(|t| t.elapsed() >= SPEC_REFRESH) {
            specs = apps
                .iter()
                .filter_map(|a| describe_app(home, a.name.clone()).ok().map(|d| *d.1))
                .collect();
            specs_read = Some(Instant::now());
        }
        let alerts = match client::request(home, &Request::Alerts) {
            Ok(Response::Alerts { open, .. }) => open,
            _ => Vec::new(),
        };
        let text = frame(&apps, &alerts, &latest_logs(&specs));
        let mut stdout = std::io::stdout().lock();
        if live {
            // Home, clear to the end: redraw in place without flicker.
            let _ = write!(
                stdout,
                "\x1b[H\x1b[2J{} {}\n\n{text}",
                ui::bold_cyan("vexor monit"),
                ui::dim("· refreshed every second · Ctrl-C to quit")
            );
        } else {
            let _ = write!(stdout, "{text}");
        }
        let _ = stdout.flush();
        drop(stdout);
        if !live {
            return ExitCode::SUCCESS;
        }
        std::thread::sleep(Duration::from_secs(1));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bars_fill_in_proportion() {
        assert_eq!(bar(0.0, 4), "····");
        assert_eq!(bar(0.5, 4), "██··");
        assert_eq!(bar(2.0, 4), "████");
    }
}

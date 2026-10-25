//! `vexor timeline`: one app's history on one time axis in the terminal
//! (plan 6b.6): CPU and memory, requests, 5xx and p95 latency, and what
//! happened (crashes, restarts, reloads, health, alerts), column by column.

use std::process::ExitCode;

use serde_json::Value;
use vexor_proto::{MetricsPoint, Request, Response, RoutePoint};

use crate::describe::{bytes, human_duration, now_ms};
use crate::metrics::sparkline;
use crate::routes::ms;
use crate::{client, code, fail, ui};

/// Columns at most.
const WIDTH: u64 = 48;
/// Events listed under the chart.
const LISTED: usize = 10;

/// Marker for an event worth seeing on the axis, and its priority when
/// several share a column. Priority 1 marks without listing: a process
/// starting, which shows restarts and fork-mode reloads but also follows
/// every crash.
fn marker(e: &Value) -> Option<(char, u8)> {
    match e["type"].as_str()? {
        "state" => match e["state"].as_str()? {
            "errored" => Some(('✗', 7)),
            "backoff" => Some(('✗', 6)),
            "starting" => Some(('↺', 1)),
            _ => None,
        },
        "spawn_error" => Some(('✗', 6)),
        "alert" => match e["level"].as_str()? {
            "recovered" => None,
            _ => Some(('▲', 5)),
        },
        "health" if e["ok"] == false => Some(('!', 4)),
        "reload" => Some(('↻', 3)),
        "memory_restart" | "watch_restart" | "cron_restart" => Some(('↺', 3)),
        "app_added" | "scaled" => Some(('●', 2)),
        _ => None,
    }
}

/// Listed under the chart and counted as notable.
fn listed(e: &Value) -> bool {
    marker(e).is_some_and(|(_, p)| p > 1)
}

/// Column of `ts` among `n` covering `since..until`.
fn column(ts: u64, since: u64, until: u64, n: usize) -> usize {
    let span = until.saturating_sub(since).max(1);
    let i = ts.saturating_sub(since).min(span - 1) * n as u64 / span;
    usize::try_from(i).unwrap_or(n - 1).min(n - 1)
}

/// Values spread over `n` columns, combined per column with `f`.
fn spread(
    values: impl Iterator<Item = (u64, f64)>,
    since: u64,
    until: u64,
    n: usize,
    f: fn(f64, f64) -> f64,
) -> Vec<Option<f64>> {
    let mut out = vec![None; n];
    for (ts, v) in values {
        if ts < since.saturating_sub(1) || ts > until {
            continue;
        }
        let slot = &mut out[column(ts, since, until, n)];
        *slot = Some(slot.map_or(v, |s| f(s, v)));
    }
    out
}

/// Buckets `[ts, ts + width)` laid over `n` columns: with `share`, each
/// column gets the part of a bucket's value it overlaps (counts), else the
/// largest value of any bucket it overlaps (latencies).
fn covered(
    buckets: impl Iterator<Item = (u64, f64)>,
    width: u64,
    since: u64,
    until: u64,
    n: usize,
    share: bool,
) -> Vec<Option<f64>> {
    let span = until.saturating_sub(since).max(1);
    let col = |i: usize| since + span * i as u64 / n as u64;
    let mut out: Vec<Option<f64>> = vec![None; n];
    for (ts, v) in buckets {
        // The current bucket only covers time up to now.
        let end = (ts + width).min(until.max(ts + 1));
        let width = end - ts;
        for (i, slot) in out.iter_mut().enumerate() {
            let overlap = end.min(col(i + 1)).saturating_sub(ts.max(col(i)));
            if overlap == 0 {
                continue;
            }
            #[allow(clippy::cast_precision_loss)]
            let part = if share {
                v * overlap as f64 / width as f64
            } else {
                v
            };
            *slot = Some(match *slot {
                None => part,
                Some(s) if share => s + part,
                Some(s) => s.max(part),
            });
        }
    }
    out
}

/// Per column, the sum over instances of each instance's largest value.
fn per_app(
    points: &[MetricsPoint],
    since: u64,
    until: u64,
    n: usize,
    value: fn(&MetricsPoint) -> Option<f64>,
) -> Vec<Option<f64>> {
    let mut instances: Vec<u32> = points.iter().map(|p| p.instance).collect();
    instances.sort_unstable();
    instances.dedup();
    let mut out = vec![None; n];
    for i in instances {
        let cols = spread(
            points
                .iter()
                .filter(|p| p.instance == i)
                .filter_map(|p| Some((p.ts_ms, value(p)?))),
            since,
            until,
            n,
            f64::max,
        );
        for (o, c) in out.iter_mut().zip(cols) {
            if let Some(c) = c {
                *o = Some(o.unwrap_or(0.0) + c);
            }
        }
    }
    out
}

/// Columns with no data count as zero (for request counts, where no
/// bucket means no requests).
fn zero_filled(cols: Vec<Option<f64>>) -> Vec<Option<f64>> {
    cols.into_iter().map(|c| Some(c.unwrap_or(0.0))).collect()
}

/// `12:00 ·····  12:30  ····· now` under the chart columns.
fn axis(since: u64, until: u64, n: usize) -> String {
    use chrono::TimeZone;
    let label = |ms: u64| {
        chrono::Local
            .timestamp_millis_opt(i64::try_from(ms).unwrap_or(0))
            .single()
            .map_or_else(String::new, |t| {
                if until - since > 2 * 24 * 3_600_000 {
                    t.format("%m-%d").to_string()
                } else {
                    t.format("%H:%M").to_string()
                }
            })
    };
    let start = label(since);
    let end = "now";
    if n < start.len() + end.len() + 1 {
        return format!("{start:<n$}");
    }
    let mid = label(since + (until - since) / 2);
    let mut line = vec![' '; n];
    let mut put = |at: usize, s: &str| {
        for (i, c) in s.chars().enumerate() {
            if let Some(slot) = line.get_mut(at + i) {
                *slot = c;
            }
        }
    };
    put(0, &start);
    if n >= start.len() + mid.len() + end.len() + 4 {
        put(n / 2 - mid.len() / 2, &mid);
    }
    put(n - end.len(), end);
    line.into_iter().collect()
}

pub struct Data {
    pub app: String,
    pub since: u64,
    pub until: u64,
    pub metrics: Vec<MetricsPoint>,
    pub metrics_resolution: u64,
    pub totals: Vec<RoutePoint>,
    pub routes_resolution: u64,
    pub events: Vec<Value>,
}

/// The chart, one line per row: label, columns, summary.
pub fn render(d: &Data) -> Vec<String> {
    let (since, until) = (d.since, d.until);
    // Columns follow the resource samples (5 s apart when raw); route
    // buckets (a minute or more) are spread over the columns they cover.
    let step = if d.metrics.is_empty() {
        5_000
    } else {
        d.metrics_resolution.max(1)
    };
    let n = usize::try_from((until.saturating_sub(since) / step).clamp(1, WIDTH)).unwrap_or(1);
    let mut lines = vec![format!(
        "{} {}",
        ui::app(&d.app),
        ui::dim(&format!(
            "· last {} · {} per column",
            human_duration(until.saturating_sub(since)),
            human_duration(until.saturating_sub(since) / n as u64)
        ))
    )];
    let row = |label: &str, cols: String, summary: String| {
        format!(
            "  {} {}  {}",
            ui::cyan(&format!("{label:<9}")),
            cols,
            ui::dim(&summary)
        )
    };
    let peak = |cols: &[Option<f64>]| {
        cols.iter()
            .flatten()
            .copied()
            .fold(None, |m: Option<f64>, v| Some(m.map_or(v, |m| m.max(v))))
    };

    if d.metrics.is_empty() {
        lines.push(row("CPU", " ".repeat(n), "no samples".to_owned()));
    } else {
        let cpu = per_app(&d.metrics, since, until, n, |p| p.cpu_max);
        let mem = per_app(&d.metrics, since, until, n, |p| {
            #[allow(clippy::cast_precision_loss)]
            Some(p.rss_max as f64)
        });
        lines.push(row(
            "CPU",
            ui::green(&sparkline(&cpu)),
            peak(&cpu).map_or_else(|| "-".to_owned(), |v| format!("peak {v:.1}%")),
        ));
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        lines.push(row(
            "Memory",
            ui::magenta(&sparkline(&mem)),
            peak(&mem).map_or_else(|| "-".to_owned(), |v| format!("peak {}", bytes(v as u64))),
        ));
    }

    if d.totals.is_empty() {
        lines.push(row(
            "Requests",
            " ".repeat(n),
            "no route data (vexor routes says why)".to_owned(),
        ));
    } else {
        let res = d.routes_resolution.max(1);
        #[allow(clippy::cast_precision_loss)]
        let reqs = zero_filled(covered(
            d.totals.iter().map(|p| (p.ts_ms, p.count as f64)),
            res,
            since,
            until,
            n,
            true,
        ));
        #[allow(clippy::cast_precision_loss)]
        let errs = zero_filled(covered(
            d.totals.iter().map(|p| (p.ts_ms, p.server_errors as f64)),
            res,
            since,
            until,
            n,
            true,
        ));
        let p95 = covered(
            d.totals
                .iter()
                .filter(|p| p.count > 0)
                .map(|p| (p.ts_ms, p.p95_ms)),
            res,
            since,
            until,
            n,
            false,
        );
        let total: u64 = d.totals.iter().map(|p| p.count).sum();
        let failed: u64 = d.totals.iter().map(|p| p.server_errors).sum();
        lines.push(row(
            "Requests",
            ui::green(&sparkline(&reqs)),
            format!("{total} total"),
        ));
        // Errors drawn against their own peak would make one error look
        // like an outage: blank columns stay blank.
        let err_line: String = errs
            .iter()
            .zip(sparkline(&errs).chars())
            .map(|(v, c)| if v.unwrap_or(0.0) > 0.0 { c } else { '·' })
            .collect();
        lines.push(row(
            "5xx",
            if failed > 0 {
                ui::red(&err_line)
            } else {
                ui::dim(&err_line)
            },
            format!("{failed} ({})", crate::routes::pct(failed, total)),
        ));
        lines.push(row(
            "p95",
            ui::yellow(&sparkline(&p95)),
            peak(&p95).map_or_else(|| "-".to_owned(), |v| format!("worst {}", ms(v))),
        ));
    }

    let mut marks: Vec<Option<(char, u8)>> = vec![None; n];
    let mut notable = 0;
    for e in &d.events {
        let (Some(m), Some(ts)) = (marker(e), e["ts_ms"].as_u64()) else {
            continue;
        };
        if ts < since || ts > until {
            continue;
        }
        if m.1 > 1 {
            notable += 1;
        }
        let slot = &mut marks[column(ts, since, until, n)];
        if slot.is_none_or(|(_, p)| m.1 > p) {
            *slot = Some(m);
        }
    }
    let event_line: String = marks
        .iter()
        .map(|m| {
            let s = m.map_or_else(|| "·".to_owned(), |(c, _)| c.to_string());
            match m.map(|(c, _)| c) {
                Some('✗') => ui::red(&s),
                Some('▲' | '!') => ui::yellow(&s),
                Some(_) => ui::cyan(&s),
                None => ui::dim(&s),
            }
        })
        .collect();
    lines.push(row("Events", event_line, format!("{notable} notable")));
    lines.push(format!("  {:<9} {}", "", ui::dim(&axis(since, until, n))));
    lines.push(ui::dim(
        "  ✗ crash  ▲ alert  ! health failing  ↺ restart  ↻ reload  ● started or scaled",
    ));
    lines
}

pub fn run(
    home: &std::path::Path,
    app: String,
    since: std::time::Duration,
    json: bool,
) -> ExitCode {
    let until = now_ms();
    let since_ms = until.saturating_sub(u64::try_from(since.as_millis()).unwrap_or(u64::MAX));
    let ask = |req: Request| match client::request(home, &req) {
        Ok(Response::Error { message, .. }) => Err((message, code::FAILED)),
        Ok(r) => Ok(r),
        Err(e) => Err((e, code::DAEMON_UNREACHABLE)),
    };
    let result = (|| {
        let (metrics_resolution, metrics, mut warnings) = match ask(Request::Metrics {
            app: app.clone(),
            since_ms,
            until_ms: until,
        })? {
            Response::Metrics {
                resolution_ms,
                points,
                warnings,
            } => (resolution_ms, points, warnings),
            _ => return Err(("unexpected answer from the daemon".to_owned(), code::FAILED)),
        };
        let (routes_resolution, totals) = match ask(Request::Routes {
            app: app.clone(),
            since_ms,
            until_ms: until,
        })? {
            Response::Routes {
                resolution_ms,
                totals,
                ..
            } => (resolution_ms, totals),
            _ => (60_000, Vec::new()),
        };
        let events = match ask(Request::Events {
            app: Some(app.clone()),
            since_ms: Some(since_ms),
            limit: 2_000,
        })? {
            Response::Events {
                events,
                warnings: w,
            } => {
                warnings.extend(w);
                events
            }
            _ => Vec::new(),
        };
        warnings.dedup();
        Ok((
            Data {
                app: app.clone(),
                since: since_ms,
                until,
                metrics,
                metrics_resolution,
                totals,
                routes_resolution,
                events,
            },
            warnings,
        ))
    })();
    let (data, warnings) = match result {
        Ok(r) => r,
        Err((msg, c)) => {
            eprintln!("vexor: {msg}");
            return ExitCode::from(c);
        }
    };
    for w in &warnings {
        eprintln!("vexor: warning: {w}");
    }
    let notable: Vec<Value> = data.events.iter().filter(|e| listed(e)).cloned().collect();
    if json {
        let out = serde_json::json!({
            "metrics_resolution_ms": data.metrics_resolution,
            "metrics": data.metrics,
            "routes_resolution_ms": data.routes_resolution,
            "requests": data.totals,
            "events": notable,
        });
        return match serde_json::to_string_pretty(&out) {
            Ok(s) => {
                println!("{s}");
                ExitCode::SUCCESS
            }
            Err(e) => fail(&e.to_string()),
        };
    }
    for line in render(&data) {
        println!("{line}");
    }
    if !notable.is_empty() {
        println!();
        let from = notable.len().saturating_sub(LISTED);
        crate::print_events(&notable[from..]);
    }
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn point(instance: u32, ts_ms: u64, cpu: f64, rss: u64) -> MetricsPoint {
        MetricsPoint {
            instance,
            ts_ms,
            pid: 1,
            samples: 1,
            cpu_avg: Some(cpu),
            cpu_max: Some(cpu),
            rss_avg: rss,
            rss_max: rss,
            fds_max: None,
        }
    }

    #[test]
    fn instances_add_up_per_column() {
        let ps = [
            point(0, 0, 10.0, 1),
            point(0, 500, 30.0, 1),
            point(1, 400, 5.0, 1),
            point(1, 3_000, 7.0, 1),
        ];
        let c = per_app(&ps, 0, 4_000, 4, |p| p.cpu_max);
        assert_eq!(c, vec![Some(35.0), None, None, Some(7.0)]);
    }

    #[test]
    fn markers_pick_the_worst_event() {
        assert_eq!(
            marker(&json!({"type":"state","state":"errored"})),
            Some(('✗', 7))
        );
        assert!(!listed(&json!({"type":"state","state":"starting"})));
        assert_eq!(marker(&json!({"type":"state","state":"online"})), None);
        assert_eq!(marker(&json!({"type":"health","ok":false})), Some(('!', 4)));
        assert_eq!(marker(&json!({"type":"health","ok":true})), None);
        assert_eq!(marker(&json!({"type":"alert","level":"recovered"})), None);
        assert_eq!(
            marker(&json!({"type":"alert","level":"danger"})),
            Some(('▲', 5))
        );
        let d = Data {
            app: "web".into(),
            since: 0,
            until: 48_000,
            metrics: vec![point(0, 0, 1.0, 1)],
            metrics_resolution: 1_000,
            totals: Vec::new(),
            routes_resolution: 60_000,
            events: vec![
                json!({"type":"reload","ts_ms":10_000}),
                json!({"type":"state","state":"backoff","ts_ms":10_400}),
                json!({"type":"health","ok":false,"ts_ms":30_000}),
            ],
        };
        let text = render(&d).join("\n");
        let events = text.lines().find(|l| l.contains("Events")).expect("row");
        assert!(
            events.contains('✗') && events.contains('!') && !events.contains('↻'),
            "{events}"
        );
        assert!(events.contains("3 notable"), "{events}");
    }

    #[test]
    fn buckets_spread_over_the_columns_they_cover() {
        // 60 requests in 0..60 s; the window starts at 30 s, 20 s columns:
        // the first column overlaps 20 s of it, the second 10 s.
        let c = covered([(0, 60.0)].into_iter(), 60_000, 30_000, 150_000, 6, true);
        assert_eq!(c, vec![Some(20.0), Some(10.0), None, None, None, None]);
        let c = covered(
            [(0, 7.0), (60_000, 3.0)].into_iter(),
            60_000,
            0,
            120_000,
            4,
            false,
        );
        assert_eq!(c, vec![Some(7.0), Some(7.0), Some(3.0), Some(3.0)]);
    }

    #[test]
    fn axis_labels_fit_the_columns() {
        let a = axis(0, 3_600_000, 48);
        assert_eq!(a.chars().count(), 48);
        assert!(a.ends_with("now"));
        assert_eq!(axis(0, 600_000, 5).chars().count(), 5);
    }
}

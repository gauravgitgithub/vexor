//! `vexor routes`: an app's per-route request stats, read from its JSON
//! request logs (docs/routes.md).

use vexor_proto::{LogLines, RoutePoint, RouteSummary};

use crate::describe::human_duration;
use crate::metrics::sparkline;
use crate::ui;

/// Routes shown without `--all`.
const TOP: usize = 20;
/// Sparkline columns.
const WIDTH: u64 = 24;

/// `0.4ms`, `12ms`, `1.2s`.
pub fn ms(v: f64) -> String {
    if v < 1.0 {
        format!("{v:.1}ms")
    } else if v < 1_000.0 {
        format!("{v:.0}ms")
    } else {
        format!("{:.1}s", v / 1_000.0)
    }
}

pub fn pct(part: u64, total: u64) -> String {
    if part == 0 {
        return "0".to_owned();
    }
    #[allow(clippy::cast_precision_loss)]
    let p = part as f64 * 100.0 / total.max(1) as f64;
    if p < 0.1 {
        "<0.1%".to_owned()
    } else {
        format!("{p:.1}%")
    }
}

/// Requests per column over `since..until`.
fn columns(points: &[&RoutePoint], since: u64, until: u64, resolution: u64) -> Vec<Option<f64>> {
    let span = until.saturating_sub(since).max(1);
    let n = (span / resolution.max(1)).clamp(1, WIDTH);
    let mut out = vec![Some(0.0); usize::try_from(n).unwrap_or(1)];
    for p in points {
        let i = p.ts_ms.saturating_sub(since).min(span - 1) * n / span;
        if let Some(Some(c)) = out.get_mut(usize::try_from(i).unwrap_or(0)) {
            #[allow(clippy::cast_precision_loss)]
            {
                *c += p.count as f64;
            }
        }
    }
    out
}

/// Why there is nothing to show, from what the shim has read.
pub fn no_data(logs: Option<LogLines>) -> &'static str {
    match logs {
        None => "No route data in this window.",
        Some(l) if l.lines == 0 => {
            "No route data: the app has written no output since its supervisor connected."
        }
        Some(l) if l.json_lines == 0 => {
            "No route data: the app's logs aren't JSON. Per-route stats come from JSON request logs (pino-http, winston, morgan JSON); see docs/routes.md."
        }
        Some(l) if l.requests == 0 => {
            "No route data: the logs are JSON, but no line has a route, an HTTP status and a duration. Map the fields with `routes` in the ecosystem file; see docs/routes.md."
        }
        Some(_) => "No route data in this window (requests seen earlier are outside it).",
    }
}

pub struct View<'a> {
    pub app: &'a str,
    pub routes: &'a [RouteSummary],
    pub points: &'a [RoutePoint],
    pub resolution_ms: u64,
    pub since: u64,
    pub until: u64,
    pub logs: Option<LogLines>,
    pub all: bool,
}

pub fn print(v: &View<'_>) {
    println!(
        "{} {}",
        ui::app(v.app),
        ui::dim(&format!(
            "· last {} · per {}",
            human_duration(v.until.saturating_sub(v.since)),
            human_duration(v.resolution_ms)
        ))
    );
    if v.routes.is_empty() {
        println!("{}", ui::dim(no_data(v.logs)));
        return;
    }
    let mut t = ui::Table::new(&[
        "Method",
        "Route",
        "Requests",
        "4xx",
        "5xx",
        "p50",
        "p95",
        "Max",
        "Requests over time",
    ])
    .right(&[2, 3, 4, 5, 6, 7])
    .shrink(1);
    let shown = if v.all { v.routes.len() } else { TOP };
    for (i, r) in v.routes.iter().enumerate().take(shown) {
        let index = u32::try_from(i).unwrap_or(u32::MAX);
        let ps: Vec<&RoutePoint> = v.points.iter().filter(|p| p.route == index).collect();
        let spark = if ps.is_empty() {
            String::new()
        } else {
            ui::green(&sparkline(&columns(&ps, v.since, v.until, v.resolution_ms)))
        };
        let server = pct(r.server_errors, r.count);
        t.row(vec![
            ui::cyan(&r.method),
            r.route.clone(),
            r.count.to_string(),
            pct(r.client_errors, r.count),
            if r.server_errors > 0 {
                ui::red(&server)
            } else {
                server
            },
            ms(r.p50_ms),
            ms(r.p95_ms),
            ms(r.max_ms),
            spark,
        ]);
    }
    t.print();
    let total: u64 = v.routes.iter().map(|r| r.count).sum();
    let mut notes = vec![format!("{total} requests")];
    if v.routes.len() > shown {
        notes.push(format!("{} more routes (--all)", v.routes.len() - shown));
    }
    notes.push("p50/p95 within ~9%".to_owned());
    println!("{}", ui::dim(&notes.join(" · ")));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats() {
        assert_eq!(ms(0.42), "0.4ms");
        assert_eq!(ms(12.4), "12ms");
        assert_eq!(ms(1_250.0), "1.2s");
        assert_eq!(pct(0, 10), "0");
        assert_eq!(pct(1, 3), "33.3%");
        assert_eq!(pct(1, 10_000), "<0.1%");
    }

    #[test]
    fn requests_spread_over_columns() {
        let p = |ts_ms, count| RoutePoint {
            route: 0,
            ts_ms,
            count,
            server_errors: 0,
            p95_ms: 1.0,
        };
        let ps = [p(0, 5), p(60_000, 7), p(59 * 60_000, 9)];
        let refs: Vec<&RoutePoint> = ps.iter().collect();
        let c = columns(&refs, 0, 3_600_000, 60_000);
        assert_eq!(c.len(), 24);
        assert_eq!((c[0], c[1], c[23]), (Some(12.0), Some(0.0), Some(9.0)));
        // Fewer buckets than columns: one column per bucket.
        assert_eq!(columns(&refs, 0, 300_000, 60_000).len(), 5);
    }

    #[test]
    fn explains_missing_data() {
        let l = |lines, json_lines, requests| {
            Some(LogLines {
                lines,
                json_lines,
                requests,
            })
        };
        assert!(no_data(l(0, 0, 0)).contains("no output"));
        assert!(no_data(l(9, 0, 0)).contains("aren't JSON"));
        assert!(no_data(l(9, 9, 0)).contains("no line has a route"));
        assert!(no_data(None).contains("this window"));
    }
}

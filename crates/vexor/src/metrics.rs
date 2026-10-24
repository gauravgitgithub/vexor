//! `vexor metrics`: an app's resource history, per instance: averages,
//! peaks, restarts in the window and a sparkline for CPU and memory.

use vexor_proto::MetricsPoint;

use crate::describe::{bytes, human_duration};
use crate::ui;

/// Sparkline columns.
const WIDTH: usize = 48;
const BARS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

/// One character per column, scaled to the largest value; `' '` where a
/// column has no data.
pub fn sparkline(columns: &[Option<f64>]) -> String {
    let max = columns.iter().flatten().copied().fold(0.0_f64, f64::max);
    columns
        .iter()
        .map(|c| match c {
            None => ' ',
            Some(_) if max <= 0.0 => BARS[0],
            Some(v) => {
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                let i = ((v / max) * 7.0).round() as usize;
                BARS[i.min(7)]
            }
        })
        .collect()
}

/// Columns for `points` over `since..until`: at most `WIDTH`, and none
/// narrower than 1.5 sample intervals, so steady sampling leaves no holes.
fn width(points: &[&MetricsPoint], since: u64, until: u64) -> usize {
    let mut gaps: Vec<u64> = points
        .windows(2)
        .map(|w| w[1].ts_ms.saturating_sub(w[0].ts_ms))
        .collect();
    gaps.sort_unstable();
    let Some(&spacing) = gaps.get(gaps.len() / 2) else {
        return 1;
    };
    let fit = until.saturating_sub(since) * 2 / (spacing * 3).max(1);
    usize::try_from(fit).unwrap_or(WIDTH).clamp(1, WIDTH)
}

/// Spread `points` over `width` columns covering `since..until`, keeping
/// each column's largest `value`.
fn columns(
    points: &[&MetricsPoint],
    since: u64,
    until: u64,
    value: impl Fn(&MetricsPoint) -> Option<f64>,
) -> Vec<Option<f64>> {
    let n = width(points, since, until);
    let mut out = vec![None; n];
    let span = until.saturating_sub(since).max(1);
    for p in points {
        let Some(v) = value(p) else { continue };
        let offset = p.ts_ms.saturating_sub(since).min(span - 1);
        let i = usize::try_from(offset * n as u64 / span).unwrap_or(n - 1);
        let slot = &mut out[i.min(n - 1)];
        *slot = Some(slot.map_or(v, |s: f64| s.max(v)));
    }
    out
}

pub fn print(app: &str, points: &[MetricsPoint], resolution_ms: u64, since: u64, until: u64) {
    println!(
        "{} {}",
        ui::app(app),
        ui::dim(&format!(
            "· last {} · {} points",
            human_duration(until.saturating_sub(since)),
            if resolution_ms <= 5_000 {
                "raw".to_owned()
            } else {
                human_duration(resolution_ms)
            }
        ))
    );
    if points.is_empty() {
        println!(
            "{}",
            ui::dim("No samples in this window yet (they are taken every 5s while an app runs).")
        );
        return;
    }
    let mut instances: Vec<u32> = points.iter().map(|p| p.instance).collect();
    instances.dedup();
    for i in instances {
        let ps: Vec<&MetricsPoint> = points.iter().filter(|p| p.instance == i).collect();
        let samples: u64 = ps.iter().map(|p| p.samples).sum::<u64>().max(1);
        let weighted = |f: &dyn Fn(&MetricsPoint) -> Option<f64>| {
            let (sum, n) = ps.iter().fold((0.0, 0u64), |(s, n), p| match f(p) {
                Some(v) => (s + v * p.samples as f64, n + p.samples),
                None => (s, n),
            });
            (n > 0).then(|| sum / n as f64)
        };
        let cpu_avg = weighted(&|p| p.cpu_avg);
        let cpu_max = ps
            .iter()
            .filter_map(|p| p.cpu_max)
            .fold(None, |m: Option<f64>, v| Some(m.map_or(v, |m| m.max(v))));
        #[allow(clippy::cast_precision_loss)]
        let rss_avg = weighted(&|p| Some(p.rss_avg as f64)).unwrap_or(0.0);
        let rss_max = ps.iter().map(|p| p.rss_max).max().unwrap_or(0);
        let fds_max = ps.iter().filter_map(|p| p.fds_max).max();
        let mut pids: Vec<u32> = ps.iter().map(|p| p.pid).collect();
        pids.dedup();
        let restarts = pids.len().saturating_sub(1);

        println!("\n  {}", ui::bold(&format!("#{i}")));
        let pct = |v: Option<f64>| v.map_or_else(|| "-".to_owned(), |v| format!("{v:.1}%"));
        println!(
            "  {} {}  {}",
            ui::cyan(&format!("{:<8}", "CPU")),
            ui::green(&sparkline(&columns(&ps, since, until, |p| p.cpu_max))),
            ui::dim(&format!("avg {} · max {}", pct(cpu_avg), pct(cpu_max)))
        );
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let rss_avg = rss_avg as u64;
        #[allow(clippy::cast_precision_loss)]
        let mem = columns(&ps, since, until, |p| Some(p.rss_max as f64));
        println!(
            "  {} {}  {}",
            ui::cyan(&format!("{:<8}", "Memory")),
            ui::magenta(&sparkline(&mem)),
            ui::dim(&format!("avg {} · max {}", bytes(rss_avg), bytes(rss_max)))
        );
        let mut extra = vec![format!("{samples} samples")];
        if let Some(f) = fds_max {
            extra.push(format!("up to {f} open files"));
        }
        extra.push(match restarts {
            0 => "no restarts".to_owned(),
            1 => "1 restart".to_owned(),
            n => format!("{n} restarts"),
        });
        println!("  {}", ui::dim(&format!("{:<8} {}", "", extra.join(" · "))));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sparkline_scales_to_the_peak_and_marks_gaps() {
        assert_eq!(
            sparkline(&[Some(0.0), Some(50.0), None, Some(100.0)]),
            "▁▅ █"
        );
        assert_eq!(sparkline(&[Some(0.0), Some(0.0)]), "▁▁");
        assert_eq!(sparkline(&[None, None]), "  ");
    }

    #[test]
    fn columns_keep_the_largest_value_per_slot() {
        let point = |ts_ms, cpu| MetricsPoint {
            instance: 0,
            ts_ms,
            pid: 1,
            samples: 1,
            cpu_avg: Some(cpu),
            cpu_max: Some(cpu),
            rss_avg: 0,
            rss_max: 0,
            fds_max: None,
        };
        // One sample a second for 96 s: 48 columns of 2 s, none empty.
        let ps: Vec<MetricsPoint> = (0..96).map(|s| point(s * 1000, s as f64)).collect();
        let refs: Vec<&MetricsPoint> = ps.iter().collect();
        let c = columns(&refs, 0, 96_000, |p| p.cpu_max);
        assert_eq!(c.len(), WIDTH);
        assert!(c.iter().all(Option::is_some), "{c:?}");
        assert_eq!((c[0], c[WIDTH - 1]), (Some(1.0), Some(95.0)));
        // Sparse samples over the same window: fewer, filled columns.
        let sparse: Vec<&MetricsPoint> = refs.iter().step_by(8).copied().collect();
        let c = columns(&sparse, 0, 96_000, |p| p.cpu_max);
        assert_eq!(c.len(), 8);
        assert!(c.iter().all(Option::is_some), "{c:?}");
    }
}

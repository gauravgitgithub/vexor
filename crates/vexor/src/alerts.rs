//! `vexor alerts`: open incidents, recent alert history and the rules in
//! force (docs/plan.md 6b.3).

use serde_json::Value;

use crate::describe::{bytes, human_duration, now_ms};
use crate::ui;

/// A rule's value in its unit: bytes, % of a core, restarts an hour, or
/// yes/no for the on/off checks.
pub fn value(rule: &str, v: f64) -> String {
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    match rule {
        "memory" | "log_disk" => bytes(v as u64),
        "cpu" => format!("{v:.1}%"),
        "restarts" => format!("{v}/h"),
        "crash_loop" | "unhealthy" => if v >= 1.0 { "yes" } else { "no" }.into(),
        _ => format!("{v}"),
    }
}

pub fn level(l: &str) -> String {
    match l {
        "danger" => ui::bold_red(l),
        "warning" | "flapping" => ui::yellow(l),
        "recovered" => ui::green(l),
        _ => l.to_owned(),
    }
}

/// One line describing a rule's thresholds.
fn rule_text(name: &str, r: &Value) -> String {
    if r["enabled"] != true {
        return ui::dim("off");
    }
    let wait = r["for_ms"].as_u64().filter(|f| *f > 0);
    let when = |l: &str| {
        let t = r[l].as_f64()?;
        Some(match name {
            "crash_loop" => format!("{l} when it gives up restarting"),
            "unhealthy" => format!("{l} when its health check fails"),
            _ => format!("{l} at {}", value(name, t)),
        })
    };
    let mut parts: Vec<String> = ["warning", "danger"]
        .iter()
        .filter_map(|l| when(l))
        .collect();
    if let Some(f) = wait {
        parts.push(format!("for {}", human_duration(f)));
    }
    if let Some(apps) = r["apps"].as_array().filter(|a| !a.is_empty()) {
        let names: Vec<&str> = apps.iter().filter_map(Value::as_str).collect();
        parts.push(format!("apps: {}", names.join(", ")));
    }
    parts.join(" · ")
}

pub fn print(open: &[Value], recent: &[Value], rules: &Value) {
    let now = now_ms();
    ui::assign_app_colors(
        open.iter()
            .chain(recent)
            .filter_map(|e| e["app"].as_str().or(e["key"]["app"].as_str())),
    );
    println!("{}", ui::bold("Open incidents"));
    if open.is_empty() {
        println!("  {}", ui::dim("none"));
    } else {
        let mut t = ui::Table::new(&["Level", "App", "#", "Rule", "Value", "Since"]).right(&[2, 4]);
        for o in open {
            let rule = o["key"]["metric"].as_str().unwrap_or("?");
            let mut lvl = level(o["level"].as_str().unwrap_or("?"));
            if o["flapping"] == true {
                lvl = format!("{lvl} {}", ui::yellow("(flapping)"));
            }
            t.row(vec![
                lvl,
                ui::app(o["key"]["app"].as_str().unwrap_or("?")),
                o["key"]["instance"]
                    .as_u64()
                    .map_or_else(|| "-".into(), |i| i.to_string()),
                rule.to_owned(),
                value(rule, o["value"].as_f64().unwrap_or(0.0)),
                human_duration(now.saturating_sub(o["since_ms"].as_u64().unwrap_or(now))),
            ]);
        }
        t.print();
    }

    println!("\n{}", ui::bold("Recent"));
    if recent.is_empty() {
        println!("  {}", ui::dim("no alerts in the last 7 days"));
    } else {
        let mut t = ui::Table::new(&["When", "App", "#", "Rule", "Level", "Value"]).right(&[2]);
        for e in recent.iter().rev().take(20).rev() {
            let rule = e["rule"].as_str().unwrap_or("?");
            let ago = human_duration(now.saturating_sub(e["ts_ms"].as_u64().unwrap_or(now)));
            let mut lvl = level(e["level"].as_str().unwrap_or("?"));
            if e["notify"] == false {
                lvl = format!("{lvl} {}", ui::dim("(quiet)"));
            }
            t.row(vec![
                format!("{ago} ago"),
                ui::app(e["app"].as_str().unwrap_or("?")),
                e["instance"]
                    .as_u64()
                    .map_or_else(|| "-".into(), |i| i.to_string()),
                rule.to_owned(),
                lvl,
                value(rule, e["value"].as_f64().unwrap_or(0.0)),
            ]);
        }
        t.print();
    }

    println!("\n{}", ui::bold("Rules"));
    if let Some(map) = rules["rules"].as_object() {
        for (name, r) in map {
            println!(
                "  {} {}",
                ui::cyan(&format!("{name:<11}")),
                rule_text(name, r)
            );
        }
    }
    println!("\n{}", ui::bold("Channels"));
    let channels = &rules["channels"];
    let mut any = false;
    if let Some(e) = channels["email"].as_object() {
        any = true;
        let to: Vec<&str> = e["to"]
            .as_array()
            .map_or_else(Vec::new, |a| a.iter().filter_map(Value::as_str).collect());
        println!(
            "  {} {} via {}:{} ({})",
            ui::cyan(&format!("{:<11}", "email")),
            to.join(", "),
            e["host"].as_str().unwrap_or("?"),
            e["port"],
            e["tls"].as_str().unwrap_or("?")
        );
    }
    for w in channels["webhooks"].as_array().into_iter().flatten() {
        any = true;
        let signed = if w["secret"].is_string() {
            ", signed"
        } else {
            ""
        };
        println!(
            "  {} {} ({}{signed})",
            ui::cyan(&format!("{:<11}", "webhook")),
            w["target"].as_str().unwrap_or("?"),
            w["format"].as_str().unwrap_or("json")
        );
    }
    if !any {
        println!(
            "  {}",
            ui::dim(
                "none: alerts are recorded only (see docs/alerts.md to add email or a webhook)"
            )
        );
    }
    println!(
        "  {}",
        ui::dim(
            "Change them in alerts.json in the Vexor home (re-read on change); `vexor alerts test` sends a test."
        )
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_read_in_their_unit() {
        assert_eq!(value("memory", 1_572_864.0), "1.5 MiB");
        assert_eq!(value("cpu", 82.25), "82.2%");
        assert_eq!(value("restarts", 4.0), "4/h");
        assert_eq!(value("crash_loop", 1.0), "yes");
    }

    #[test]
    fn rules_read_naturally() {
        let r = serde_json::json!({
            "enabled": true, "warning": 3.0, "danger": 10.0, "for_ms": 0, "apps": []
        });
        assert_eq!(rule_text("restarts", &r), "warning at 3/h · danger at 10/h");
        let off = serde_json::json!({ "enabled": false });
        assert!(rule_text("cpu", &off).contains("off"));
    }
}

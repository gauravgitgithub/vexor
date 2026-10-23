//! Terminal presentation: colours, status badges and tables.
//!
//! Colour is used when stdout is a terminal (or `--color always`), never
//! with `NO_COLOR` or `--color never`. Tables use box-drawing characters on
//! a terminal and plain aligned columns otherwise, so output piped to
//! scripts stays easy to parse.

use std::io::IsTerminal;
use std::sync::OnceLock;

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum ColorChoice {
    Auto,
    Always,
    Never,
}

static COLOR: OnceLock<bool> = OnceLock::new();
static FANCY: OnceLock<bool> = OnceLock::new();

pub fn init(choice: ColorChoice) {
    let tty = std::io::stdout().is_terminal();
    let env_off = std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty())
        || std::env::var("TERM").is_ok_and(|t| t == "dumb");
    let env_on = std::env::var_os("FORCE_COLOR").is_some_and(|v| !v.is_empty() && v != "0")
        || std::env::var_os("CLICOLOR_FORCE").is_some_and(|v| !v.is_empty() && v != "0");
    let color = match choice {
        ColorChoice::Always => true,
        ColorChoice::Never => false,
        ColorChoice::Auto => env_on || (tty && !env_off),
    };
    let _ = COLOR.set(color);
    let _ = FANCY.set(tty || choice == ColorChoice::Always);
}

/// Colours on.
pub fn color() -> bool {
    *COLOR.get().unwrap_or(&false)
}

/// Box-drawn tables and symbols (a terminal is reading).
pub fn fancy() -> bool {
    *FANCY.get().unwrap_or(&false)
}

fn paint(s: &str, code: &str) -> String {
    if color() && !s.is_empty() {
        format!("\x1b[{code}m{s}\x1b[0m")
    } else {
        s.to_owned()
    }
}

pub fn bold(s: &str) -> String {
    paint(s, "1")
}
pub fn dim(s: &str) -> String {
    paint(s, "2")
}
pub fn red(s: &str) -> String {
    paint(s, "31")
}
pub fn green(s: &str) -> String {
    paint(s, "32")
}
pub fn yellow(s: &str) -> String {
    paint(s, "33")
}
pub fn cyan(s: &str) -> String {
    paint(s, "36")
}
pub fn magenta(s: &str) -> String {
    paint(s, "35")
}
pub fn bold_red(s: &str) -> String {
    paint(s, "1;31")
}
pub fn bold_green(s: &str) -> String {
    paint(s, "1;32")
}
pub fn bold_yellow(s: &str) -> String {
    paint(s, "1;33")
}
pub fn bold_cyan(s: &str) -> String {
    paint(s, "1;36")
}

/// Distinct, readable foreground colours (256-colour palette) for app names.
const APP_COLORS: [u8; 16] = [
    39, 208, 170, 114, 220, 75, 204, 150, 141, 81, 179, 117, 213, 109, 215, 147,
];

static ASSIGNED: std::sync::Mutex<Vec<(String, u8)>> = std::sync::Mutex::new(Vec::new());

fn hash_color(name: &str) -> usize {
    let h = name.bytes().fold(2_166_136_261u32, |h, b| {
        (h ^ u32::from(b)).wrapping_mul(16_777_619)
    });
    h as usize % APP_COLORS.len()
}

/// Give the apps shown together distinct colours: each takes its own
/// colour (from its name), or the next free one if another app has it.
/// Names are taken in sorted order, so the same set always looks the same.
pub fn assign_app_colors<'a>(names: impl IntoIterator<Item = &'a str>) {
    let mut names: Vec<&str> = names.into_iter().collect();
    names.sort_unstable();
    names.dedup();
    let mut map = ASSIGNED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    map.clear();
    let mut used = [false; APP_COLORS.len()];
    for n in names {
        let mut i = hash_color(n);
        if used.iter().all(|u| *u) {
            used = [false; APP_COLORS.len()];
        }
        while used[i] {
            i = (i + 1) % APP_COLORS.len();
        }
        used[i] = true;
        map.push((n.to_owned(), APP_COLORS[i]));
    }
}

/// An app name in its colour.
pub fn app(name: &str) -> String {
    let assigned = ASSIGNED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .iter()
        .find(|(n, _)| n == name)
        .map(|(_, c)| *c);
    let c = assigned.unwrap_or(APP_COLORS[hash_color(name)]);
    paint(name, &format!("1;38;5;{c}"))
}

/// A coloured state badge: `● online`.
pub fn state(s: &str) -> String {
    let dot = if fancy() { "● " } else { "" };
    let text = format!("{dot}{s}");
    match s {
        "online" => green(&text),
        "starting" | "stopping" | "launching" | "reloading" => yellow(&text),
        "backoff" | "waiting restart" | "unhealthy" => bold_yellow(&text),
        "errored" | "error" | "failed" => bold_red(&text),
        "stopped" | "-" => dim(&text),
        _ => text,
    }
}

/// Symbols with plain fallbacks.
pub fn ok_mark() -> String {
    green(if fancy() { "✔" } else { "ok" })
}
pub fn fail_mark() -> String {
    bold_red(if fancy() { "✖" } else { "error" })
}
pub fn warn_mark() -> String {
    yellow(if fancy() { "▲" } else { "warning" })
}
pub fn info_mark() -> String {
    cyan(if fancy() { "›" } else { "-" })
}

/// Visible width: ANSI escapes don't count.
pub fn width(s: &str) -> usize {
    let mut w = 0;
    let mut esc = false;
    for c in s.chars() {
        if esc {
            if c.is_ascii_alphabetic() {
                esc = false;
            }
        } else if c == '\x1b' {
            esc = true;
        } else {
            w += 1;
        }
    }
    w
}

/// Cut to `max` visible characters with `…`, keeping colour codes intact.
pub fn truncate(s: &str, max: usize) -> String {
    if width(s) <= max || max == 0 {
        return s.to_owned();
    }
    let mut out = String::new();
    let mut w = 0;
    let mut esc = false;
    for c in s.chars() {
        if esc {
            out.push(c);
            if c.is_ascii_alphabetic() {
                esc = false;
            }
            continue;
        }
        if c == '\x1b' {
            esc = true;
            out.push(c);
            continue;
        }
        if w + 1 >= max {
            out.push('…');
            break;
        }
        out.push(c);
        w += 1;
    }
    if color() {
        out.push_str("\x1b[0m");
    }
    out
}

pub fn pad(s: &str, w: usize) -> String {
    format!("{s}{}", " ".repeat(w.saturating_sub(width(s))))
}

fn pad_right(s: &str, w: usize) -> String {
    format!("{}{s}", " ".repeat(w.saturating_sub(width(s))))
}

/// Terminal columns, when stdout is a terminal.
pub fn term_width() -> Option<usize> {
    if !std::io::stdout().is_terminal() {
        return None;
    }
    rustix::termios::tcgetwinsize(std::io::stdout())
        .ok()
        .map(|w| usize::from(w.ws_col))
        .filter(|w| *w > 20)
}

/// A table: headers, rows, right-aligned columns, and the column that
/// shrinks (with `…`) when the terminal is too narrow.
pub struct Table {
    headers: Vec<String>,
    rows: Vec<Vec<String>>,
    right: Vec<bool>,
    shrink: usize,
}

impl Table {
    pub fn new(headers: &[&str]) -> Self {
        Table {
            headers: headers.iter().map(|h| (*h).to_owned()).collect(),
            rows: Vec::new(),
            right: vec![false; headers.len()],
            shrink: 0,
        }
    }

    pub fn right(mut self, cols: &[usize]) -> Self {
        for &c in cols {
            self.right[c] = true;
        }
        self
    }

    pub fn shrink(mut self, col: usize) -> Self {
        self.shrink = col;
        self
    }

    pub fn row(&mut self, cells: Vec<String>) {
        self.rows.push(cells);
    }

    pub fn print(&self) {
        print!("{}", self.render());
    }

    pub fn render(&self) -> String {
        let n = self.headers.len();
        let mut widths: Vec<usize> = (0..n)
            .map(|c| {
                self.rows
                    .iter()
                    .map(|r| r.get(c).map_or(0, |s| width(s)))
                    .chain([width(&self.headers[c])])
                    .max()
                    .unwrap_or(0)
            })
            .collect();
        let fancy = fancy();
        // Fit the terminal: shrink one column, never below 8.
        if let Some(tw) = term_width() {
            let frame = if fancy { 3 * n + 1 } else { 2 * (n - 1) };
            let total: usize = widths.iter().sum::<usize>() + frame;
            if total > tw {
                let w = &mut widths[self.shrink];
                *w = w.saturating_sub(total - tw).max(8);
            }
        }
        let cell = |s: &str, c: usize| {
            let t = truncate(s, widths[c]);
            if self.right[c] {
                pad_right(&t, widths[c])
            } else {
                pad(&t, widths[c])
            }
        };
        let mut out = String::new();
        if fancy {
            let line = |l: &str, m: &str, r: &str| {
                let segs: Vec<String> = widths.iter().map(|w| "─".repeat(w + 2)).collect();
                dim(&format!("{l}{}{r}", segs.join(m)))
            };
            let bar = dim("│");
            out.push_str(&line("╭", "┬", "╮"));
            out.push('\n');
            let head: Vec<String> = (0..n).map(|c| bold(&cell(&self.headers[c], c))).collect();
            out.push_str(&format!("{bar} {} {bar}\n", head.join(&format!(" {bar} "))));
            out.push_str(&line("├", "┼", "┤"));
            out.push('\n');
            for r in &self.rows {
                let cells: Vec<String> = (0..n)
                    .map(|c| cell(r.get(c).map_or("", String::as_str), c))
                    .collect();
                out.push_str(&format!(
                    "{bar} {} {bar}\n",
                    cells.join(&format!(" {bar} "))
                ));
            }
            out.push_str(&line("╰", "┴", "╯"));
            out.push('\n');
        } else {
            let head: Vec<String> = (0..n).map(|c| cell(&self.headers[c], c)).collect();
            out.push_str(head.join("  ").trim_end());
            out.push('\n');
            for r in &self.rows {
                let cells: Vec<String> = (0..n)
                    .map(|c| cell(r.get(c).map_or("", String::as_str), c))
                    .collect();
                out.push_str(cells.join("  ").trim_end());
                out.push('\n');
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn widths_ignore_colour_codes() {
        assert_eq!(width("\x1b[1;32mabc\x1b[0m"), 3);
        assert_eq!(width("● on"), 4);
    }

    #[test]
    fn plain_tables_align() {
        let mut t = Table::new(&["NAME", "N"]).right(&[1]);
        t.row(vec!["a".into(), "10".into()]);
        t.row(vec!["long".into(), "2".into()]);
        assert_eq!(t.render(), "NAME   N\na     10\nlong   2\n");
    }

    #[test]
    fn truncation() {
        assert_eq!(truncate("abcdefgh", 5), "abcd…");
        assert_eq!(truncate("abc", 5), "abc");
    }
}

//! Per-route request stats from the app's JSON logs (plan 6b.5,
//! docs/routes.md).
//!
//! The log writer threads hand every chunk of output to the [`Tap`], off
//! the shim's runtime thread. A line counts as one request when it is a
//! JSON object with a route (or URL), an HTTP status and a duration, found
//! under the configured paths or the usual names (pino-http, express-winston,
//! morgan JSON, OpenTelemetry). Fastify logs the URL and the outcome on two
//! lines; they are joined by request id, per process. Nothing is guessed
//! from plain-text lines: they are only counted.
//!
//! The control connection takes one [`RouteWindow`] per interval.

use std::collections::{HashMap, VecDeque};
use std::sync::{Mutex, PoisonError};

use serde_json::Value;
use vexor_core::RouteSpec;
use vexor_proto::{RouteStats, RouteWindow, hist};

/// Longest line read for request fields; longer lines are only counted.
const MAX_LINE: usize = 64 * 1024;
/// Fastify-style requests waiting for their outcome line.
const MAX_PENDING: usize = 4096;
/// Longer request ids aren't joined (bounds what waiting requests hold).
const MAX_ID_LEN: usize = 128;
const MAX_ROUTE_LEN: usize = 200;
const OTHER: &str = "(other)";

const ROUTE: &[&str] = &[
    "route",
    "req.route",
    "http.route",
    "req.originalUrl",
    "req.url",
    "url",
    "path",
    "request.url",
    "meta.req.originalUrl",
    "meta.req.url",
    "http.target",
    "http.url",
];
const METHOD: &[&str] = &[
    "req.method",
    "method",
    "request.method",
    "meta.req.method",
    "http.method",
    "http.request.method",
];
const STATUS: &[&str] = &[
    "res.statusCode",
    "res.status",
    "statusCode",
    "status",
    "status_code",
    "response.status",
    "response.statusCode",
    "meta.res.statusCode",
    "http.status_code",
    "http.response.status_code",
];
const DURATION: &[&str] = &[
    "responseTime",
    "response_time",
    "response-time",
    "res.responseTime",
    "duration",
    "duration_ms",
    "elapsed",
    "elapsed_ms",
    "latency",
    "latency_ms",
    "meta.responseTime",
    "http.duration",
];
const REQUEST_ID: &[&str] = &["reqId", "req.id", "requestId", "request_id"];

/// Dotted paths, tried in order.
struct Paths(Vec<Vec<String>>);

impl Paths {
    fn new(configured: Option<&String>, defaults: &[&str]) -> Paths {
        let split = |p: &str| p.split('.').map(str::to_owned).collect();
        Paths(match configured {
            Some(p) => vec![split(p)],
            None => defaults.iter().map(|p| split(p)).collect(),
        })
    }

    fn find<'a>(&self, v: &'a Value) -> Option<&'a Value> {
        self.0.iter().find_map(|path| {
            path.iter()
                .try_fold(v, |v, key| v.get(key))
                .filter(|v| !v.is_null())
        })
    }
}

#[derive(Default)]
struct Acc {
    count: u64,
    client_errors: u64,
    server_errors: u64,
    total_ms: f64,
    max_ms: f64,
    hist: Vec<(u16, u64)>,
}

#[derive(Default)]
struct State {
    start_ms: u64,
    lines: u64,
    json_lines: u64,
    routes: HashMap<(String, String), Acc>,
    /// (pipe, request id) of requests whose outcome is on a later line.
    pending: HashMap<(u32, String), (String, String)>,
    pending_order: VecDeque<(u32, String)>,
}

pub struct Tap {
    route: Paths,
    method: Paths,
    status: Paths,
    duration: Paths,
    request_id: Paths,
    /// A line holding none of these can't be part of a request: skipped
    /// without parsing (most app log lines).
    needles: Vec<Vec<u8>>,
    unit_ms: f64,
    max_routes: usize,
    state: Mutex<State>,
}

/// A line still being read from one pipe.
#[derive(Default)]
pub struct Partial {
    buf: Vec<u8>,
    /// The line outgrew [`MAX_LINE`]: skip to its end.
    skipping: bool,
}

/// What one line said.
enum Line {
    Text,
    Json,
    Request {
        method: String,
        route: String,
        status: u16,
        ms: f64,
    },
}

impl Tap {
    pub fn new(spec: &RouteSpec, now_ms: u64) -> Tap {
        Tap {
            route: Paths::new(spec.route.as_ref(), ROUTE),
            method: Paths::new(spec.method.as_ref(), METHOD),
            status: Paths::new(spec.status.as_ref(), STATUS),
            duration: Paths::new(spec.duration.as_ref(), DURATION),
            request_id: Paths::new(None, REQUEST_ID),
            needles: needles(spec.status.as_deref()),
            unit_ms: spec.duration_unit.ms(),
            max_routes: usize::try_from(spec.max_routes.max(1)).unwrap_or(usize::MAX),
            state: Mutex::new(State {
                start_ms: now_ms,
                ..State::default()
            }),
        }
    }

    /// Read the complete lines in `data`, output of `pipe`, keeping an
    /// unfinished last line in `partial`.
    pub fn feed(&self, pipe: u32, partial: &mut Partial, data: &[u8]) {
        let mut found = Vec::new();
        let mut rest = data;
        while let Some(end) = rest.iter().position(|&b| b == b'\n') {
            let (head, tail) = rest.split_at(end);
            rest = &tail[1..];
            if partial.skipping {
                partial.skipping = false;
                found.push(Line::Text);
            } else if partial.buf.is_empty() {
                found.push(self.parse(pipe, head));
            } else {
                partial.buf.extend_from_slice(head);
                let line = std::mem::take(&mut partial.buf);
                found.push(self.parse(pipe, &line));
            }
        }
        if !partial.skipping && !rest.is_empty() {
            if partial.buf.len() + rest.len() > MAX_LINE {
                partial.buf = Vec::new();
                partial.skipping = true;
            } else {
                partial.buf.extend_from_slice(rest);
            }
        }
        if found.is_empty() {
            return;
        }
        let mut s = self.lock();
        for line in found {
            s.lines += 1;
            let (method, route, status, ms) = match line {
                Line::Text => continue,
                Line::Json => {
                    s.json_lines += 1;
                    continue;
                }
                Line::Request {
                    method,
                    route,
                    status,
                    ms,
                } => (method, route, status, ms),
            };
            s.json_lines += 1;
            let mut key = (method, route);
            if !s.routes.contains_key(&key) && s.routes.len() >= self.max_routes {
                key = ("-".to_owned(), OTHER.to_owned());
            }
            let acc = s.routes.entry(key).or_default();
            acc.count += 1;
            match status {
                400..=499 => acc.client_errors += 1,
                500..=599 => acc.server_errors += 1,
                _ => {}
            }
            acc.total_ms += ms;
            acc.max_ms = acc.max_ms.max(ms);
            hist::add(&mut acc.hist, hist::bucket(ms), 1);
        }
    }

    /// Everything read since the previous window, or `None` when the app
    /// wrote nothing.
    pub fn take(&self, now_ms: u64) -> Option<RouteWindow> {
        let mut s = self.lock();
        if s.lines == 0 {
            s.start_ms = now_ms;
            return None;
        }
        let mut routes: Vec<RouteStats> = s
            .routes
            .drain()
            .map(|((method, route), a)| RouteStats {
                method,
                route,
                count: a.count,
                client_errors: a.client_errors,
                server_errors: a.server_errors,
                total_ms: a.total_ms,
                max_ms: a.max_ms,
                hist: a.hist,
            })
            .collect();
        routes.sort_by(|a, b| (&a.route, &a.method).cmp(&(&b.route, &b.method)));
        let window = RouteWindow {
            start_ms: s.start_ms,
            end_ms: now_ms,
            lines: s.lines,
            json_lines: s.json_lines,
            routes,
        };
        s.start_ms = now_ms;
        s.lines = 0;
        s.json_lines = 0;
        Some(window)
    }

    /// A pipe closed: forget its requests still waiting for an outcome.
    pub fn closed(&self, pipe: u32) {
        let mut s = self.lock();
        s.pending.retain(|(p, _), _| *p != pipe);
        s.pending_order.retain(|(p, _)| *p != pipe);
    }

    fn parse(&self, pipe: u32, line: &[u8]) -> Line {
        let line = line.trim_ascii();
        if !line.starts_with(b"{") || line.len() > MAX_LINE {
            return Line::Text;
        }
        if !self.needles.iter().any(|n| contains(line, n)) {
            return if line.ends_with(b"}") {
                Line::Json
            } else {
                Line::Text
            };
        }
        let Ok(v) = serde_json::from_slice::<Value>(line) else {
            return Line::Text;
        };
        if !v.is_object() {
            return Line::Text;
        }
        let route = self.route.find(&v).and_then(Value::as_str).map(normalize);
        let method = self
            .method
            .find(&v)
            .and_then(Value::as_str)
            .map(|m| vexor_proto::http_method(m).to_owned());
        let status = self.status.find(&v).and_then(status);
        let ms = self
            .duration
            .find(&v)
            .and_then(number)
            .map(|d| d * self.unit_ms)
            .filter(|ms| ms.is_finite() && *ms >= 0.0);
        // With pino's `pid`: cluster workers share one pipe, and each
        // numbers its requests from req-1.
        let id = || {
            let id = match self.request_id.find(&v)? {
                Value::String(s) => s.clone(),
                Value::Number(n) => n.to_string(),
                _ => return None,
            };
            if id.len() > MAX_ID_LEN {
                return None;
            }
            Some(match v.get("pid").and_then(Value::as_u64) {
                Some(pid) => format!("{pid}:{id}"),
                None => id,
            })
        };
        match (route, status, ms) {
            (Some(route), Some(status), Some(ms)) => Line::Request {
                method: method.unwrap_or_else(|| "-".to_owned()),
                route,
                status,
                ms,
            },
            // Fastify: the outcome line has no URL; the incoming line had it.
            (None, Some(status), Some(ms)) => match id().and_then(|id| self.resolve(pipe, id)) {
                Some((method, route)) => Line::Request {
                    method,
                    route,
                    status,
                    ms,
                },
                None => Line::Json,
            },
            (Some(route), None, None) => {
                if let Some(id) = id() {
                    self.remember(pipe, id, method.unwrap_or_else(|| "-".to_owned()), route);
                }
                Line::Json
            }
            _ => Line::Json,
        }
    }

    fn remember(&self, pipe: u32, id: String, method: String, route: String) {
        let mut s = self.lock();
        let key = (pipe, id);
        if s.pending.insert(key.clone(), (method, route)).is_none() {
            s.pending_order.push_back(key);
        }
        while s.pending_order.len() > MAX_PENDING {
            if let Some(old) = s.pending_order.pop_front() {
                s.pending.remove(&old);
            }
        }
    }

    fn resolve(&self, pipe: u32, id: String) -> Option<(String, String)> {
        let mut s = self.lock();
        let key = (pipe, id);
        let found = s.pending.remove(&key)?;
        s.pending_order.retain(|k| *k != key);
        Some(found)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Keys a request's lines have: the status (outcome lines) or a request id
/// (Fastify's incoming lines).
fn needles(status: Option<&str>) -> Vec<Vec<u8>> {
    let last = |p: &str| p.rsplit('.').next().unwrap_or(p).as_bytes().to_vec();
    let mut out: Vec<Vec<u8>> = match status {
        Some(p) => vec![last(p)],
        None => vec![b"status".to_vec()],
    };
    out.extend(["reqId", "requestId", "request_id"].map(|k| k.as_bytes().to_vec()));
    out
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    let Some(&first) = needle.first() else {
        return false;
    };
    let mut from = 0;
    while let Some(at) = haystack[from..].iter().position(|&b| b == first) {
        if haystack[from + at..].starts_with(needle) {
            return true;
        }
        from += at + 1;
    }
    false
}

/// A number, or a string holding one (`"12.5"`, `"12.5ms"`).
fn number(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().trim_end_matches("ms").trim().parse().ok(),
        _ => None,
    }
}

fn status(v: &Value) -> Option<u16> {
    let n = number(v)?;
    if !(100.0..600.0).contains(&n) || n.fract() != 0.0 {
        return None;
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    Some(n as u16)
}

/// A route from a template or URL: no scheme, host, credentials, query or
/// fragment, ids (and anything with `@`, such as an email) replaced by
/// `:id`, control characters replaced, at most 200 bytes.
pub fn normalize(raw: &str) -> String {
    let mut path = raw.trim();
    // `scheme://authority/...` (any scheme, any case) or `//authority/...`.
    let scheme_end = path.find("://").filter(|&i| {
        let s = &path.as_bytes()[..i];
        s.first().is_some_and(u8::is_ascii_alphabetic)
            && s.iter()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'+' | b'.' | b'-'))
    });
    let authority = match scheme_end {
        Some(i) => Some(&path[i + 3..]),
        None => path.strip_prefix("//"),
    };
    if let Some(rest) = authority {
        path = rest.find('/').map_or("/", |i| &rest[i..]);
    }
    let end = path.find(['?', '#']).unwrap_or(path.len());
    let path = &path[..end];
    let mut out = String::with_capacity(path.len());
    for (i, seg) in path.split('/').enumerate() {
        if i > 0 {
            out.push('/');
        }
        if is_id(seg) {
            out.push_str(":id");
        } else {
            out.extend(seg.chars().map(|c| if c.is_control() { '?' } else { c }));
        }
    }
    if out.is_empty() {
        out.push('/');
    }
    if out.len() > MAX_ROUTE_LEN {
        let mut cut = MAX_ROUTE_LEN;
        while !out.is_char_boundary(cut) {
            cut -= 1;
        }
        out.truncate(cut);
    }
    out
}

/// Numbers, UUIDs, long hex strings and long tokens with digits.
fn is_id(seg: &str) -> bool {
    let b = seg.as_bytes();
    if b.is_empty() {
        return false;
    }
    if b.contains(&b'@') {
        return true;
    }
    let uuid = b.len() == 36
        && b.iter().enumerate().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => *c == b'-',
            _ => c.is_ascii_hexdigit(),
        });
    b.iter().all(u8::is_ascii_digit)
        || uuid
        || (b.len() >= 16 && b.iter().all(u8::is_ascii_hexdigit))
        || (b.len() >= 20
            && b.iter().any(u8::is_ascii_digit)
            && b.iter()
                .all(|c| c.is_ascii_alphanumeric() || *c == b'-' || *c == b'_'))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tap() -> Tap {
        Tap::new(&RouteSpec::default(), 1_000)
    }

    fn feed(t: &Tap, text: &str) {
        t.feed(0, &mut Partial::default(), text.as_bytes());
    }

    fn route<'a>(w: &'a RouteWindow, method: &str, route: &str) -> &'a RouteStats {
        w.routes
            .iter()
            .find(|r| r.method == method && r.route == route)
            .unwrap_or_else(|| panic!("no {method} {route} in {:?}", w.routes))
    }

    #[test]
    fn urls_become_routes() {
        assert_eq!(normalize("/users/123/posts?x=1#y"), "/users/:id/posts");
        assert_eq!(
            normalize("https://api.example.com/o/550e8400-e29b-41d4-a716-446655440000"),
            "/o/:id"
        );
        assert_eq!(normalize("/files/deadbeefdeadbeef00"), "/files/:id");
        assert_eq!(normalize("/t/abcDEF123456789xyz_-Q"), "/t/:id");
        assert_eq!(normalize("/users/:id"), "/users/:id");
        assert_eq!(normalize("HTTPS://user:pw@h.example/a?t=1"), "/a");
        assert_eq!(normalize("ws+unix://h/sock/x"), "/sock/x");
        assert_eq!(normalize("//cdn.example/lib.js"), "/lib.js");
        assert_eq!(
            normalize("/invite/jane@example.com/accept"),
            "/invite/:id/accept"
        );
        assert_eq!(normalize("/a:b://not-a-scheme"), "/a:b://not-a-scheme");
        assert_eq!(normalize("/v1/health"), "/v1/health");
        assert_eq!(normalize(""), "/");
        assert_eq!(normalize("/a\u{1b}[31m"), "/a?[31m");
        assert_eq!(normalize(&format!("/{}", "é".repeat(300))).len(), 199);
    }

    #[test]
    fn reads_pino_http_express_winston_and_morgan_lines() {
        let t = tap();
        feed(
            &t,
            concat!(
                r#"{"level":30,"req":{"id":1,"method":"GET","url":"/users/42"},"res":{"statusCode":200},"responseTime":12,"msg":"request completed"}"#,
                "\n",
                r#"{"meta":{"req":{"url":"/users/7","method":"get"},"res":{"statusCode":503},"responseTime":30},"level":"info"}"#,
                "\n",
                r#"{"method":"POST","url":"/login","status":"401","response-time":"4.5"}"#,
                "\n",
                "plain text line\n",
                r#"{"level":30,"msg":"server listening"}"#,
                "\n",
            ),
        );
        let w = t.take(6_000).expect("window");
        assert_eq!((w.start_ms, w.end_ms), (1_000, 6_000));
        assert_eq!((w.lines, w.json_lines), (5, 4));
        let users = route(&w, "GET", "/users/:id");
        assert_eq!((users.count, users.server_errors), (2, 1));
        assert!((users.total_ms - 42.0).abs() < 1e-9);
        assert!((users.max_ms - 30.0).abs() < 1e-9);
        let login = route(&w, "POST", "/login");
        assert_eq!((login.count, login.client_errors), (1, 1));
        // The window was taken: the next one starts empty.
        assert!(t.take(7_000).is_none());
    }

    #[test]
    fn fastify_lines_are_joined_per_process() {
        let t = tap();
        let incoming = |id: &str, url: &str| {
            format!(
                r#"{{"reqId":"{id}","req":{{"method":"GET","url":"{url}"}},"msg":"incoming request"}}"#
            ) + "\n"
        };
        let done = |id: &str, code: u16| {
            format!(
                r#"{{"reqId":"{id}","res":{{"statusCode":{code}}},"responseTime":2.5,"msg":"request completed"}}"#
            ) + "\n"
        };
        let (mut a, mut b) = (Partial::default(), Partial::default());
        // Two processes, both numbering requests from req-1.
        t.feed(1, &mut a, incoming("req-1", "/a").as_bytes());
        t.feed(2, &mut b, incoming("req-1", "/b").as_bytes());
        t.feed(2, &mut b, done("req-1", 500).as_bytes());
        t.feed(1, &mut a, done("req-1", 200).as_bytes());
        // An outcome without its incoming line is not a request.
        t.feed(1, &mut a, done("req-9", 200).as_bytes());
        let w = t.take(2_000).expect("window");
        assert_eq!(route(&w, "GET", "/a").server_errors, 0);
        assert_eq!(route(&w, "GET", "/b").server_errors, 1);
        assert_eq!(w.routes.len(), 2);
        assert_eq!(w.json_lines, 5);
        // Cluster workers share a pipe; pino's pid tells them apart.
        let mut c = Partial::default();
        let with_pid =
            |pid: u32, line: String| line.replacen('{', &format!(r#"{{"pid":{pid},"#), 1);
        t.feed(3, &mut c, with_pid(10, incoming("req-1", "/x")).as_bytes());
        t.feed(3, &mut c, with_pid(11, incoming("req-1", "/y")).as_bytes());
        t.feed(3, &mut c, with_pid(11, done("req-1", 500)).as_bytes());
        t.feed(3, &mut c, with_pid(10, done("req-1", 200)).as_bytes());
        let w = t.take(3_000).expect("window");
        assert_eq!(route(&w, "GET", "/x").server_errors, 0);
        assert_eq!(route(&w, "GET", "/y").server_errors, 1);
        // A closed pipe's waiting requests are forgotten.
        t.feed(1, &mut a, incoming("req-2", "/a").as_bytes());
        t.closed(1);
        assert!(t.lock().pending.is_empty() && t.lock().pending_order.is_empty());
        // Overlong ids aren't held.
        t.feed(
            1,
            &mut a,
            incoming(&"x".repeat(MAX_ID_LEN + 1), "/a").as_bytes(),
        );
        assert!(t.lock().pending.is_empty());
    }

    #[test]
    fn lines_split_across_chunks_and_overlong_lines() {
        let t = tap();
        let mut p = Partial::default();
        let line = r#"{"url":"/x","status":200,"duration":1}"#;
        let (head, tail) = line.split_at(10);
        t.feed(0, &mut p, head.as_bytes());
        t.feed(0, &mut p, format!("{tail}\n").as_bytes());
        // Longer than MAX_LINE: counted, skipped, and the next line reads.
        t.feed(0, &mut p, &vec![b'{'; MAX_LINE + 1]);
        t.feed(0, &mut p, b"tail\n");
        assert!(p.buf.is_empty() && !p.skipping);
        t.feed(0, &mut p, format!("{line}\n").as_bytes());
        let w = t.take(2_000).expect("window");
        assert_eq!(w.lines, 3);
        assert_eq!(route(&w, "-", "/x").count, 2);
    }

    #[test]
    fn non_requests_are_not_guessed() {
        let t = tap();
        feed(
            &t,
            concat!(
                // Status out of range, text status, missing duration, array.
                r#"{"url":"/a","status":42,"duration":1}"#,
                "\n",
                r#"{"url":"/a","status":"ok","duration":1}"#,
                "\n",
                r#"{"url":"/a","status":200}"#,
                "\n",
                "[1,2]\n",
                r#"{"url":"/a","status":200,"duration":-3}"#,
                "\n",
            ),
        );
        let w = t.take(2_000).expect("window");
        assert!(w.routes.is_empty(), "{:?}", w.routes);
        assert_eq!((w.lines, w.json_lines), (5, 4));
    }

    #[test]
    fn configured_paths_units_and_the_route_cap() {
        let spec = RouteSpec {
            route: Some("http.path".into()),
            status: Some("http.code".into()),
            duration: Some("took".into()),
            duration_unit: vexor_core::DurationUnit::S,
            max_routes: 2,
            ..RouteSpec::default()
        };
        let t = Tap::new(&spec, 0);
        let line = |p: &str| {
            format!(r#"{{"http":{{"path":"{p}","code":204}},"took":0.25,"url":"/no"}}"#) + "\n"
        };
        for p in ["/a", "/b", "/c", "/d", "/a"] {
            feed(&t, &line(p));
        }
        let w = t.take(1).expect("window");
        assert_eq!(route(&w, "-", "/a").count, 2);
        assert_eq!(route(&w, "-", OTHER).count, 2);
        // Methods outside the standard ones are folded.
        let t = tap();
        feed(
            &t,
            "{\"method\":\"x1\",\"url\":\"/a\",\"status\":200,\"duration\":1}\n",
        );
        feed(
            &t,
            "{\"method\":\"patch\",\"url\":\"/a\",\"status\":200,\"duration\":1}\n",
        );
        let w2 = t.take(2).expect("window");
        assert_eq!(route(&w2, "-", "/a").count, 1);
        assert_eq!(route(&w2, "PATCH", "/a").count, 1);
        assert!((route(&w, "-", "/b").max_ms - 250.0).abs() < 1e-9);
        assert_eq!(w.routes.len(), 3);
    }
}

#[cfg(test)]
mod bench {
    use super::*;

    /// ns per line when `line` fills 8 KiB chunks.
    fn per_line(line: &str) -> (f64, u64) {
        let mut chunk = Vec::new();
        while chunk.len() < 8 * 1024 - line.len() {
            chunk.extend_from_slice(line.as_bytes());
            chunk.push(b'\n');
        }
        let per_chunk = chunk.iter().filter(|&&b| b == b'\n').count();
        let t = Tap::new(&RouteSpec::default(), 0);
        let mut p = Partial::default();
        let rounds = 20_000;
        let start = std::time::Instant::now();
        for _ in 0..rounds {
            t.feed(0, &mut p, &chunk);
        }
        let took = start.elapsed();
        let lines = (per_chunk * rounds) as u64;
        let requests = t
            .take(1)
            .map_or(0, |w| w.routes.iter().map(|r| r.count).sum());
        assert!(requests == 0 || requests == lines);
        #[allow(clippy::cast_precision_loss)]
        (took.as_nanos() as f64 / lines as f64, requests)
    }

    /// `cargo test --release -p vexor-shim routes_throughput -- --ignored --nocapture`
    #[test]
    #[ignore = "benchmark"]
    fn routes_throughput() {
        let request = r#"{"level":30,"time":1729760000000,"pid":4242,"hostname":"web-1","req":{"id":1,"method":"GET","url":"/users/12345?expand=orders","headers":{"host":"localhost:3000","user-agent":"curl/8.7.1","accept":"*/*"},"remoteAddress":"127.0.0.1","remotePort":51234},"res":{"statusCode":200,"headers":{"content-type":"application/json","content-length":"512"}},"responseTime":12.5,"msg":"request completed"}"#;
        let other = r#"{"level":30,"time":1729760000000,"pid":4242,"hostname":"web-1","msg":"cache refreshed","entries":1200,"took":3.2}"#;
        let text = "2026-10-25T12:00:00.000+05:30 cache refreshed: 1200 entries in 3.2ms";
        for (name, line) in [
            ("request", request),
            ("other JSON", other),
            ("plain text", text),
        ] {
            let (ns, _) = per_line(line);
            println!("{name:<10} ({:>3} bytes): {ns:>6.0} ns/line", line.len());
        }
    }
}

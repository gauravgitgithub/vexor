# Per-route stats

`vexor routes <app>` shows requests per route, with the share of 4xx and
5xx responses, p50, p95 and longest latency, and requests over time. It
reads them from the request logs the app already writes as JSON (pino-http,
Fastify, express-winston, morgan with a JSON format, OpenTelemetry-style
fields). The app needs no code change, agent or SDK.

```
$ vexor routes web --since 6h
web · last 6h 0m · per 1m 0s
Method  Route              Requests    4xx   5xx    p50    p95    Max  Requests over time
GET     /users/:id            48211      0  0.4%   20ms   33ms  1.2s   ▃▄▅▅▆█▇▆▅▄▄▃▃▄▅▆▇█▇▆▅▄▃▃
POST    /login                 9840  35.1%     0   20ms   33ms   41ms  ▂▃▃▄▄▅▅▄▃▃▂▂▂▃▄▄▅▅▄▃▃▂▂▂
GET     /search                9702      0     0  158ms  266ms  1.9s   ▃▄▅▅▆█▇▆▅▄▄▃▃▄▅▆▇█▇▆▅▄▃▃
...
```

- `--since 10m|2h|7d` (default 1h). Per-minute buckets are kept for 7 days
  and per-hour buckets for 90 days.
- `--all` shows every route, not only the 20 busiest.
- `--json` returns `routes` (one summary per route, busiest first),
  `totals` (all routes together per bucket),
  `points` (per bucket: requests, 5xx responses and p95 for the 20 busiest
  routes, `route` being an index into `routes`), `resolution_ms`, and
  `logs` (see [No route data](#no-route-data)).
- `vexor timeline <app>` draws requests, 5xx and p95 of all routes
  together next to CPU, memory and the app's events.
- Prometheus: `vexor_route_requests_total` and
  `vexor_route_server_errors_total`, labelled `app`, `method` and `route`
  ([prometheus.md](prometheus.md)).

## Which lines count

A line counts as one request when it is a JSON object with a route (or
URL), an HTTP status (100 to 599) and a duration. The method is optional.
Without configuration, these fields are looked for, first match wins:

| Field | Looked for in |
|---|---|
| route | `route`, `req.route`, `http.route`, `req.originalUrl`, `req.url`, `url`, `path`, `request.url`, `meta.req.originalUrl`, `meta.req.url`, `http.target`, `http.url` |
| method | `req.method`, `method`, `request.method`, `meta.req.method`, `http.method`, `http.request.method` |
| status | `res.statusCode`, `res.status`, `statusCode`, `status`, `status_code`, `response.status`, `response.statusCode`, `meta.res.statusCode`, `http.status_code`, `http.response.status_code` |
| duration (ms) | `responseTime`, `response_time`, `response-time`, `res.responseTime`, `duration`, `duration_ms`, `elapsed`, `elapsed_ms`, `latency`, `latency_ms`, `meta.responseTime`, `http.duration` |

Methods other than GET, POST, PUT, PATCH, DELETE, HEAD, OPTIONS, CONNECT
and TRACE are shown as `-`. Numbers given as strings (`"12.5"`, `"12.5ms"`, as morgan writes them) are
read too. Fastify logs the URL on its "incoming request" line and the
status and time on "request completed"; the two are joined by `reqId`,
separately for each process (by output pipe, and by pino's `pid` field for
cluster workers, which share one), so processes numbering requests the
same way don't mix.

URLs become routes: the scheme, host, credentials (`user:pw@`), query
string and fragment are dropped, and path segments that look like ids
(numbers, UUIDs, hex strings of 16+ characters, tokens of 20+ characters
with digits) or contain `@` (an email address) become `:id`:
`/users/42/orders?page=2` is counted as `/users/:id/orders`. A route
template in the log (`route`, `http.route`) is used as it is. Routes are
cut at 200 bytes and control characters are replaced, so a log line can't
send terminal escapes to `vexor routes`.

Plain-text lines, JSON that isn't an object, and JSON without all three
fields are never guessed at: they only count as lines.

## Mapping fields

When the app logs requests under other names, map them in the ecosystem
file (dotted paths into the JSON line):

```js
module.exports = {
  apps: [{
    name: "api",
    script: "server.js",
    routes: {
      route: "http.path",      // template or URL
      method: "http.verb",
      status: "http.code",
      duration: "took",
      duration_unit: "s",      // ns, us, ms (default) or s
      max_routes: 200,         // distinct routes per window (default 100)
    },
  }],
};
```

A mapped field is looked for only at its path; fields left out keep the
defaults above. `routes: false` turns per-route stats off for the app.

## No route data

When there is nothing to show, `vexor routes` says why, from what the
app's supervisor has read since it connected:

- the app has written no output;
- the logs aren't JSON;
- the logs are JSON, but no line has a route, a status and a duration
  (map the fields as above);
- requests were seen, but not in the queried window.

An app started under a Vexor older than 0.11 keeps its old supervisor, which
doesn't read routes; `vexor routes` says so. `vexor delete` and start the
app again to get them.

## Limits and cost

- Routes per app: `max_routes` (default 100) distinct routes per 5 s
  window, the rest counted as `(other)`; the database keeps at most 1,000
  per app, and Prometheus counts at most 1,000 per app.
- Latency quantiles come from a histogram with buckets 19% wide
  (0.1 ms to 30 min), so p50/p95/p99 are within about 9% of the exact
  value. The average and the maximum are exact.
- Lines longer than 64 KiB are counted but not read; request ids longer
  than 128 bytes are not joined.
- Route names reach `vexor routes`, `events.db` and Prometheus labels. A
  URL path that carries secrets in a segment not shaped like an id is kept
  as written: log route templates (`req.route`, `http.route`) or map
  `routes.route` to one, or set `routes: false`.
- Reading happens on the log writer thread, never on the supervisor's
  control loop. Measured on an Apple M5, release build: a 392-byte pino-http
  request line costs 2.3 µs, another JSON line 0.2 µs (most are skipped
  before parsing because they have no status or request id field), a
  plain-text line 26 ns. 1,000 requests per second cost about 0.25% of one
  core. Benchmark: `cargo test --release -p vexor-shim routes_throughput --
  --ignored --nocapture`.
- Per-route stats come from the log output, so requests are counted when
  the app logs them; output dropped because the disk couldn't keep up
  (`logs.overflow: drop`) is not counted.

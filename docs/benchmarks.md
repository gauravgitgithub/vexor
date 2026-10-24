# Benchmarks

## Results (2026-10-24)

Measured 2026-10-24 on Apple M5 (colima VM) (4 cores, 4 GB), linux/arm64 6.8.0-117-generic, in a container, Node v22.23.4. Each tool runs alone, installed from npm, on the same apps. **Bold** = best; n/a = not supported or failed (see notes).

| Measure | Vexor 0.10.1 | PM2 7.1.0 | OxMgr 0.5.0 | forever 4.0.3 |
|---|---|---|---|---|
| List apps (CLI), median | 14 ms | 58 ms | **13 ms** | 86 ms |
| Start 10 apps until online, median | 339 ms | 362 ms | **268 ms** | 1005 ms |
| Supervisor memory (PSS), 1 app | 5.5 MiB | 54 MiB | **3.4 MiB** | 30 MiB |
| Supervisor memory (PSS), 10 apps | 7.9 MiB | 37 MiB | **3.6 MiB** | 186 MiB |
| Supervisor memory (PSS), 50 apps | 19 MiB | 33 MiB | **4.5 MiB** | 820 MiB |
| Supervisor memory (RSS), 1 app | 8.2 MiB | 130 MiB | **3.4 MiB** | 67 MiB |
| Supervisor memory (RSS), 10 apps | 35 MiB | 132 MiB | **3.6 MiB** | 672 MiB |
| Supervisor memory (RSS), 50 apps | 153 MiB | 134 MiB | **4.5 MiB** | 3359 MiB |
| Supervisor CPU while idle, 1 app | **0.00%** | 0.37% | 0.03% | 0.20% |
| Supervisor CPU while idle, 10 apps | **0.07%** | 0.70% | 0.07% | 2.50% |
| Supervisor CPU while idle, 50 apps | **0.20%** | 2.10% | 0.30% | 11.77% |
| Crash to back online, median | 47 ms | 131 ms | 41 ms | **18 ms** |
| Failed requests in 5 cluster reloads | **0** | **0** | 2 | n/a |
| Supervisor daemon killed: failed requests | **0** | 730 | **0** | n/a |
| Supervisor daemon killed: app still managed | **yes** | no | **yes** | n/a |
| Installed size (npm) | 4.9 MiB | 17 MiB | **3.9 MiB** | 20 MiB |

Requests sent: reloads Vexor 1592, PM2 1565, OxMgr 1598, forever 0; daemon kill Vexor 1154, PM2 1210, OxMgr 1202, forever 0.

Notes:
- OxMgr, crash_restart_ms: only 3 restarts timed: it marks an app errored after its 3rd crash by default, even with uptime between crashes

Charts: [`docs/benchmarks/`](benchmarks/). Raw results and the run log:
[`benchmarks/results/2026-10-24-apple-m5-colima/`](../benchmarks/results/2026-10-24-apple-m5-colima/).
The previous run (Vexor 0.3.1, 2026-10-23, Apple M5 in a Docker Desktop VM
with 10 cores and 8 GB) is kept in
[`benchmarks/results/2026-10-23-apple-m5-docker/`](../benchmarks/results/2026-10-23-apple-m5-docker/);
this host is smaller (4 cores, 4 GB), so compare tools within one run.

### What was measured

- **List apps (CLI):** wall time of `<tool> ls` (forever: `forever list`) with 5 apps running, median of 15 runs after 2 warm-ups. Each tool as installed from npm, so Vexor's time includes its Node.js launcher.
- **Start 10 apps:** from the start command (an ecosystem file for Vexor and PM2; one `start` per app for OxMgr and forever) until all 10 are online, median of 5.
- **Supervisor memory and CPU:** everything the tool runs besides the apps (PM2's God daemon; Vexor's daemon and per-app shims; OxMgr's daemon; forever's per-app monitors), with 1, 10 and 50 idle apps, sampled after 3 s and over 30 s. PSS splits shared pages (one binary's code mapped by many processes) between processes and is the fair total; RSS counts them in each process.
- **Crash to back online:** `kill -9` of the app until a new process is online, 8 times with 1.5 s between (OxMgr: 3, its default crash limit, set by `crashLimit` in the harness).
- **Reload:** a 2-instance cluster HTTP app under constant load (2 clients, a request every 5 ms each) during 5 reloads; failed requests counted.
- **Supervisor killed:** `kill -9` of the daemon under the same load; failed requests over the next 3 s, and whether the next `list` still shows the app.
- **Installed size:** `du` of the globally installed package (with its platform binary).

### Reproduce

```sh
docker run --rm -e HOST_CPU="your machine" -v "$PWD/benchmarks":/b node:22-alpine sh -c '
  npm i -g pm2@7.1.0 vexord@0.10.1 oxmgr@0.5.0 forever@4.0.3 &&
  node /b/run.js --out /b/results.json'
node benchmarks/charts.js benchmarks/results.json docs/benchmarks
```

To measure an unreleased build, copy a static Linux binary (`cargo build
--release --target aarch64-unknown-linux-musl`, or x86_64) over
`$(npm root -g)/vexord/node_modules/vexord-linux-arm64/bin/vexor` before
running `run.js`.

## Methodology (Phase 1 plan)

Targets set before any measurement (engineering goals): idle supervisor RSS at least 50% lower than PM2; selected start operations at least 30% faster.

## Rules

- Same host, OS, kernel, Node binary (absolute path), app workload for both tools.
- Record: CPU model, cores, RAM, kernel, OS, PM2 version, Vexor commit, Node version, governor, container or not.
- PM2 in its default configuration plus the equivalent settings Vexor uses (e.g. if Vexor rotates logs, PM2 gets `pm2-logrotate`; the extra RSS is reported separately).
- Warm-up run discarded; ≥ 10 trials; report median, p95, min/max; raw CSV committed under `benchmarks/results/<date>-<host>/`.
- Supervisor overhead (daemon RSS/CPU, including PM2 God daemon and Vexor daemon + cluster shims) is reported separately from app usage.
- Failed targets and regressions are published too.

## Suite

| ID | Measure | Method |
|---|---|---|
| B1 | CLI latency | `hyperfine 'vexor list' 'pm2 list'` with daemon running |
| B2 | Daemon cold start → ready | time from spawn until IPC answers |
| B3 | Idle RSS/CPU, 1/10/100/1000 procs | `sleep`-style Node worker; sample `/proc/<pid>/status` VmRSS & `stat` utime every 1 s for 10 min |
| B4 | Single start latency | command issue → process `online` |
| B5 | Bulk start (100) | as B4 |
| B6 | Restart-to-ready | HTTP app with `wait_ready` |
| B7 | Crash detect → respawn | app `process.exit(1)` at known timestamp |
| B8 | Graceful stop latency | app with 500 ms SIGTERM handler |
| B9 | Log throughput | app writes 1 KiB lines flat out; bytes/s on disk, daemon CPU |
| B10 | Log back-pressure | throttled disk (cgroup io.max); measure app stall vs dropped bytes |
| B11 | 24 h soak | daemon RSS slope |
| B12 | Daemon restart recovery | kill -9 daemon; time to consistent state |
| B13 | Reload under load | `wrk2` constant rate during reload; error count + p99 |

Scripts go in `benchmarks/` and run the same on any Linux host.

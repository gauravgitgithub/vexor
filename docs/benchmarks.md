# Benchmark Methodology (Phase 1 draft, no results yet)

No performance claims exist yet. Targets (engineering goals, not claims): idle supervisor RSS at least 50% lower than PM2; selected start operations at least 30% faster.

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

# Phased Plan

Each phase ends with green tests and updated docs. Phase N+1 doesn't start while phase N has failing mandatory tests.

## Phase 2: minimal reliable vertical slice (Linux + macOS)

Goal: `vexor start app.js --name api -i 1`, `stop`, `restart`, `delete`, `list --json`, `logs`, and `vexor daemon` with crash restart + backoff. No PM2 config yet.

| Step | Deliverable | Acceptance |
|---|---|---|
| 2.1 ✅ | Workspace skeleton (`crates/vexor` bin only; other crates are added in the step that first needs them), `rust-toolchain.toml` (1.99.1), CI: fmt, clippy -D warnings, test on linux x64/arm64 + macOS, cargo-deny; actions pinned by commit SHA | all checks green locally; CI not yet run (no GitHub remote) |
| 2.2 ✅ | `vexor-core`: state machine + restart policy (pure) | property tests: no transition sequence yields unbounded restarts within the budget window; every event in every state is handled |
| 2.3 ✅ | `vexor shim`: spawn (argv exec, own process group, env/cwd), pipes, NODE_CHANNEL_FD, identity capture, restart loop | integration: env/args/cwd round-trip; metachar args not shell-split |
| 2.4 ✅ | Stop: SIGTERM → kill_timeout → SIGKILL on pgid; shim subreaper orphan reap | test app ignoring SIGTERM is killed at timeout; grandchild cleaned up |
| 2.4b ✅ | Shim ↔ daemon protocol, buffered events, reconnect, daemon reconcile | **zero-downtime proof**: HTTP app under constant load (`wrk2`), kill -9 daemon × 100 in a loop → 0 failed requests, 0 app restarts, log lines contiguous; app crash while daemon is down → still restarted by shim |
| 2.5 ✅ | CLI ↔ daemon socket (`daemon.sock`, 0600, peer UID check, JSON lines), single-daemon flock, `state.json` desired state (pulled forward from 2.6), CLI `start`/`stop`/`restart`/`delete`/`list` with daemon auto-start | stop survives daemon kill -9; 8 concurrent identical starts → 1 app; corrupt state refused and kept; mutant without persisted stop fails tests. Wrong-UID rejection is implemented but not tested (needs a second user) |
| 2.6 (mostly done) | State store (atomic `state.json`) + resurrect on daemon start (policy R1/replace) | kill -9 daemon in a loop × 100 → no duplicate app instances, state consistent |
| 2.6b ✅ | `events.db` (SQLite, WAL): writer thread with bounded queue, every shim event stored with its resume cursor in one transaction, retention (30 days / 1M rows), corrupt-file recovery, `vexor events` | daemon kill -9 mid crash loop → exactly `max_restarts` restarts, then errored; stored history contiguous by sequence number including events from the outage; corrupt db kept as `events.db.corrupt-<ts>`, app not restarted, warning shown. Crash budgets live in the shim (R2), so they survive daemon restarts without the database |
| 2.7 ✅ | Log capture + size rotation + drop policy (writer thread per log file, bounded queue, `drop`/`block` overflow, reopen after external move, write errors reported once) | 1 GB flood: ≤ 3 files of 1 MiB on disk, shim peak RSS ~6 MB, app exit 0 (macOS + Linux); block mode keeps all 200,000 lines across rotation; drop mode accounts for every byte |
| 2.8 ✅ | Legacy runtime checks in CI (`legacy-node` job): static Linux x64 musl binary, `tests/legacy/node-smoke.sh` in `node:<v>-alpine` for Node 8, 10, 12, 14, 16, 18, 20, 22, 24 | 6 checks per version (ready via IPC, natural exit with IPC open, graceful SIGTERM, crash restart + crash-loop limit, args/env/cwd/instances, readiness timeout); all pass locally on linux-arm64; CI result below |

## Phase 3: npm distribution (mostly done, not yet published)

Done:
- `vexord` launcher (ES5, no dependencies, no install scripts) + platform packages `vexord-{linux,darwin}-{x64,arm64}` with `os`/`cpu` filtering; `npm/scripts/stage.js` stamps the Cargo version and checks the package list against the launcher.
- `install.sh`: direct binary install, mandatory SHA-256 check, no sudo.
- CI on every push: static linux-x64 binary → launcher checks on Node 10–24 (npm 6, 8, 10, 11) and install.sh checksum checks; macOS launcher check.
- `release.yml` on `v*.*.*` tags: 4 targets, `SHA256SUMS`, CycloneDX SBOM, build-provenance attestations, GitHub Release (prerelease), npm publish with provenance only when the `NPM_TOKEN` secret is set.

Open:
- Reserve the npm names (needs `npm login`), add `NPM_TOKEN`, cut `v0.1.0`. The release workflow has not run yet; its first run is the real test.
- Static musl builds need a C cross compiler for the bundled SQLite (`musl-gcc`; cargo-zigbuild if cross-compiling); release binary is ~3.7 MB.
- pnpm/yarn and `--ignore-scripts` install checks.
- darwin-x64 is cross-built on an arm64 runner and not run at all yet (the release smoke test skips it); it needs an Intel runner before it can be advertised.

## Phase 4: PM2 compatibility

4.1 ✅ Ecosystem files (`crates/vexor-pm2`): ES5 loader run on the app's own Node (`.js/.cjs/.mjs`; `.json` without Node), every PM2 field classified (supported / partial / ignored / fatal; unknown fields reported), `vexor start <ecosystem> [--only] [--env NAME] [--allow-unsupported]`, `vexor check`, `autostart: false`. Tested: 21 translator/loader unit tests, 4 CLI tests, loading an ES5 config on Node 8–24.

4.2 ✅ Cluster mode: ES5 cluster primary (`crates/vexor-shim/src/cluster.js`) on the app's Node holds the shared listening sockets; the shim keeps one state machine per worker and drives workers through it (spawn / signal / events). Primary death → its group is killed and every worker restarts under its own policy. `exec_mode: cluster` in ecosystem files; `vexor start app.js -i N` is cluster mode (as in PM2), `--fork` opts out. Tested: workers share a port and are replaced individually (others keep serving), primary death restarts all, readiness per worker, graceful stop; cluster check on Node 8–24.

4.3 ✅ Health-gated `vexor reload`: reload is a phase of the core state machine (replacement spawned next to the current process; promoted when healthy; abandoned, keeping the current process, on exit, spawn failure or timeout; interrupted by stop/restart/crash). Cluster mode is start-first (healthy = `ready` with `wait_ready`, else `listening`); fork mode is stop-first unless `reload_strategy: start_first` (healthy = `ready`, else up for `min_uptime`). Old cluster workers retire gracefully (`worker.disconnect()`, then SIGTERM, SIGKILL after `kill_timeout`). Tested: 3 reloads of a 1-instance cluster app under load with 0 failed requests and exactly one worker left; failed reload keeps the old process with 0 failed requests and is reported within a second; fork default = restart; fork start-first on a fixed port fails safely (EADDRINUSE); 6 core unit tests + property tests; cluster reload on Node 8–24.

4.4 ✅ `vexor logs [name] [--lines N] [--follow] [--out|--err] [--raw]` (tail reads backwards in chunks; rotated `.1` fills in short files; follow survives rotation and truncation) and `vexor describe <name> [--json] [--show-env]` (config, live status with uptime, policy in words, log settings, recent events; env values hidden by default). `vexor list` gains an UPTIME column; event history is ordered by event time. Tested: unit tests for tail/follow/rotation/truncation; CLI tests for describe redaction and JSON, per-stream tails, and following 1,500+ lines across 3+ rotations with no gap or duplicate. A mutant follower that ignores rotation fails both.

4.5 ✅ `vexor save` / `resurrect` (explicit `dump.json` snapshot, 0600; the daemon already restores apps from its own state at every start), `vexor startup [systemd|launchd]` / `unstartup` (systemd system unit `vexor-<user>.service` with `KillMode=process`, so stopping the service never stops apps; per-user LaunchAgent with `AbandonProcessGroup`; non-root on systemd prints the exact `sudo` command). Tested: unit tests for the generated files (quoting, escaping, `plutil -lint` on macOS); CLI tests for save/resurrect (desired state kept, idempotent) and `startup --print`; a real systemd run in a container: verify, install, stop keeps the app, start reconnects to the same app, killed daemon restarted, unstartup leaves apps running. launchd install not run live (it would add a login item on the dev machine).

4.6 ✅ PM2 conformance harness (`tests/conformance`): 11 scenarios run under PM2 7.1.0 and Vexor in isolated homes, comparing what a probe app saw (runs, argv, cwd, env, instance numbers, stop signal) and what each tool reported. It found two real incompatibilities, fixed for 0.1.1: `max_restarts` off by one (and `0` = PM2 default 16), and SIGTERM instead of SIGINT on stop (new `kill_signal` setting). Result: 10 identical, 1 intentional difference (`wait_ready` without a ready signal is a failed start, PM2 says online). Report: docs/conformance.md.

## Phase 5: migration
dry-run, backup, journaled apply, rollback (docs/migration.md).

5.1 ✅ `vexor migrate --from-pm2 --dry-run`: reads `dump.pm2` (no PM2 command, no Vexor state, no daemon), groups per-instance entries into apps, drops PM2 bookkeeping and defaults, keeps PM2's log files and the saved environment, and runs the result through the ecosystem translator. Report per app: command line, cwd, logs, env variable names (never values), findings, downtime class (none / brief / blocked); global warnings for a world-readable dump, a PM2 boot service and captured shell environment. `--json`; exit 4 when some apps are blocked. Tested: 9 unit tests on a real PM2 7.1.0 dump (fixture), 2 CLI tests (redaction, no daemon or state created, exit codes); end to end against PM2 7.1.0 in a container (cluster, fork, `interpreter: none`, stopped app): all apps planned, dump unchanged, PM2's apps still running.

5.2 ✅ `vexor migrate --from-pm2 --backup`: copies PM2's state files and boot service, the plan (`plan.json`, redacted) and a manifest with SHA-256 hashes to `<home>/migrations/<id>/` (0700 dirs, 0600 files), via a temporary directory renamed when complete; never overwrites; a `--dump` outside the PM2 home is backed up alone. Tested: CLI test (two backups get distinct ids, permissions, byte-identical dump copy, manifest hashes, redacted plan, no leftovers, PM2 files unchanged, no daemon, nothing kept when the dump is missing); `cargo deny` with the new `sha2` dependency.

5.3 ✅ `vexor migrate --from-pm2 --apply`: applies the newest (or `--backup-id`) backup's plan only if PM2's dump is unchanged since (SHA-256) and the plan intact; confirmation (`--yes`); per app `pm2 stop` → Vexor start → health check (all instances online, no restart, same live processes for `min_uptime`), else `vexor delete` + `pm2 start` and stop; blocked apps stay on PM2; refuses names Vexor already has; journal `journal.jsonl` in the backup (fsync per step) for resume; lock per backup. Tested: 3 CLI tests with a recording stand-in for `pm2` (happy path + journal + idempotent rerun; unhealthy app put back under PM2; no backup / no `--yes` / changed dump / existing name / failing `pm2 stop`), journal unit test; end to end against PM2 7.1.0 (3 apps in 3 s, 2 of 456 probe requests failed during a ~240 ms cutover gap).

5.4 ✅ `vexor migrate --rollback [ID]` (newest first: delete from Vexor, `pm2 start` and wait for `online` in `pm2 jlist`; per-app confirmation or `--yes`; journaled; repeatable; `--only`) and `vexor migrate --status [ID] [--json]`. Tested: rollback-scope unit test (order, never-begun and already-rolled-back apps, re-migration), CLI test (status before/after, refused without `--yes`, rollback, second rollback no-op, migrate again); end to end against PM2 7.1.0 (apply then rollback, PM2 back to its original states, 3 of 273 probe requests failed). Found while testing: on a heavily loaded macOS host an instantly crashing app's crash can be reported after a 1 s health window (no exit is lost; the report lags), documented in docs/migration.md.

Phase 5 done: dry run, backup, apply and rollback.

## Phase 6: hardening
HTTP/TCP health checks, readiness-gated reload, max_memory_restart, watch, cron_restart, metrics endpoint, `doctor`, fault-injection suite, uid/gid review, Windows feasibility study.

## Phase 6b: observability and alerting

Goal: an advanced, self-hosted `pm2 monit`. Admins get warning, danger, and
recovered ("green") alerts, plus a timeline that helps with root-cause
analysis. PM2 offers alerting only in the paid PM2 Plus. Everything runs
locally; no SaaS.

| Step | Deliverable | Acceptance |
|---|---|---|
| 6b.1 | Per-process metrics: CPU, RSS, open FDs, restarts, uptime, health results. Sampled by the shim (default every 5 s), stored by the daemon | Sampling 100 processes adds < 1% of one core; counted as supervisor overhead in benchmark B3 |
| 6b.2 | Time-series storage in `events.db` (SQLite) with rollups: raw 5 s for 24 h, 1 min for 7 days, 1 h for 90 days; size cap | 100 processes × 30 days stays under a documented size; a 24 h chart query for one process returns in < 100 ms |
| 6b.3 | Alert rules (`vexor.alerts.yaml`): thresholds with `warning` / `danger` levels and a `for:` duration; built-in rules for crash loop, restart rate, memory, CPU, unhealthy, log disk usage | Fires once per incident, sends "recovered" when it clears; dedup, cooldown and flap damping covered by tests |
| 6b.4 | Channels: email (SMTP with STARTTLS) and webhook (JSON, HMAC-signed); Slack after that | Channel credentials are never logged or shown in `describe`/`doctor` (security.md T3); a failed delivery is retried with backoff and reported |
| 6b.5 | Per-route attribution from the app's existing structured JSON logs (pino/winston: route, status, duration). Field mapping is configurable; apps need no code changes | Error rate and p95 latency per route on the timeline; logs that aren't JSON are reported as "no route data", never guessed |
| 6b.6 | Local dashboard: one timeline per process (CPU/memory, restarts, deploys/reloads, health, error bursts per route, alerts) | Binds 127.0.0.1 by default and requires login; non-loopback needs explicit config (security.md T11) |
| 6b.7 | Export: Prometheus `/metrics`, and OpenTelemetry (OTLP) ingestion for apps that already emit traces | Works with an existing Grafana/Prometheus setup |

Decisions and limits:
- **Storage: SQLite, not DuckDB, inside the supervisor.** The workload is
  frequent small, crash-safe writes, plus range queries per process.
  DuckDB would add tens of MB of C++ to the binary, defaults to using up to
  80% of RAM, and complicates the static builds. Heavy analytics go through
  export (Parquet, Prometheus, OTLP), or an optional separate tool that
  queries the data with DuckDB.
- **Vexor sees processes, not requests.** Without the app's structured logs
  or OpenTelemetry data, it can't say which API endpoint is slow or failing,
  and it says so instead of guessing.
- **"Root cause" means a correlated timeline** (for example: deploy at 14:02 →
  memory climb → crash loop → errors on `/api/orders`). Vexor does not claim
  automatic root-cause detection.
- Alerting and the dashboard run in the daemon, not the shims, so app
  supervision never depends on them. Both can be turned off.

## Phase 7: benchmarks (docs/benchmarks.md) · Phase 8: release readiness

## Decisions needed before Phase 2

1. ~~Daemon-crash recovery~~ **Decided:** per-app shim (R2), so daemon crash or upgrade causes zero app downtime and zero log loss (architecture.md §7, which also lists where zero downtime is impossible).
2. ~~State format~~ **Decided:** `state.json` for desired state + SQLite `events.db` for history, restart budgets and migration journal (§8).
3. Cluster mode via ES5 cluster-shim on app's Node (§6).
4. ~~License~~ **Decided:** `MIT OR Apache-2.0` (LICENSE-MIT, LICENSE-APACHE). PM2 is AGPL-3.0, so Vexor is a clean-room implementation built from docs and observed behaviour; no PM2 code is copied or translated.
5. ~~npm name~~ **Decided:** `vexord` (`npm i -g vexord`), command `vexor`, platform packages `vexord-<os>-<arch>`. Names to reserve before the public release.
6. ~~Rust toolchain~~ **Done:** rustup, stable 1.99.1, minimal profile + rustfmt + clippy, pinned in `rust-toolchain.toml`. Linux musl cross targets (cargo-zigbuild) are added in Phase 3.

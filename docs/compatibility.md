# Vexor Compatibility Specification (Phase 1 draft)

Three separate questions, always answered separately:

1. **Runtime compatibility**: can Vexor launch and supervise the app?
2. **Behavioural compatibility**: does Vexor reproduce the PM2 behaviour the app relies on?
3. **Security support**: does the runtime still get upstream fixes? (Vexor warns on EOL Node and never forces an upgrade.)

Status values: `planned` (spec only), `tested` (green in CI), `qualified` (works with stated limits), `unsupported`.
Every row below is `planned` until CI says otherwise.

CI (`legacy-node` job, every push): static linux-x64 musl binary on `node:<v>-alpine` for Node 8, 10, 12, 14, 16, 18, 20, 22 and 24, six checks each: ready via `process.send('ready')`, natural exit with the IPC channel open, graceful SIGTERM, crash restart with crash-loop limit, args/env/cwd/instance variable, readiness timeout. The same checks pass on linux-arm64 (run locally). The full test suite runs in CI on Linux x64, Linux arm64 and macOS arm64 (with the runner's current Node).

Not covered yet: Node 4, 6 and 0.12 (tier 4), cluster mode (Phase 4), and the npm wrapper's own Node range (Phase 3).

## 1. Platform matrix (Layer A binary)

| Target | Build | Tier | Notes |
|---|---|---|---|
| linux-x64 (musl, static) | cross | 1 | Runs on any glibc/musl distro. pidfd needs kernel ≥ 5.3; older kernels fall back to start-time re-check. |
| linux-arm64 (musl, static) | cross | 1 | Graviton / ARM VPS |
| darwin-arm64 | native | 2 | Dev machines; launchd for `startup` |
| darwin-x64 | cross | 3 | Only if CI runner available |
| win32-x64 | — | not in v0.x | Needs named pipes, Job Objects, different signals. Not advertised until tested. |

Startup integration: systemd (Linux), launchd (macOS). OpenRC/upstart/SysV: `unsupported` in v0.x (PM2 supports them; reported clearly by `migrate`).

## 2. Managed Node.js runtime matrix (Layer C)

Docker `node:<ver>` images (pinned by digest) are used for every version listed.

| Node | Tier | CI cadence | Notes |
|---|---|---|---|
| 24, 22, 20 | 1 | every PR | current/maintenance LTS |
| 18, 16, 14 | 2 | every PR (Linux x64 only) | EOL; warning shown |
| 12, 10, 8 | 3 | nightly | EOL; ESM configs unsupported below 12 |
| 6, 4, 0.12 | 4 | weekly, best effort | `qualified`: fork mode, signals, logs, NODE_CHANNEL_FD readiness; cluster shim to be confirmed |

The per-version test list follows prompt §6.4: HTTP server, worker, exe path selection, args/env/cwd, logs, non-zero exit, crash + backoff, SIGTERM/SIGINT, kill timeout, multi-instance, daemon restart recovery, readiness, missing runtime.

PM2 baseline: PM2 7.x (7.1.0 latest, 7.0.4 installed locally) declares `engines.node >=18.0.0`. Earlier majors: v2 ≥0.12, v3 ≥6, v4 ≥8.10, v5 ≥12, v6 ≥16. Vexor's behavioural baseline is PM2 7.x semantics, while its runtime baseline reaches as far back as the PM2 v2-era Node versions. Vexor's supervisor has no Node dependency at all.

## 3. npm wrapper matrix (Layer B)

| Item | Promise |
|---|---|
| Node | ≥ 10: tested in CI on 10, 12, 14, 16, 18, 20, 22, 24 (Linux x64) and 24 (macOS arm64) |
| npm | ≥ 6: tested with npm 6, 8, 10, 11 (global install; optional platform package resolution) |
| Package managers | npm, pnpm, yarn classic; yarn berry PnP = `qualified` |
| Below the floor | direct binary install: release tarball + SHA-256 + `install.sh` |

The wrapper is a zero-dependency ES5 script with no `postinstall` and no network access.

## 4. CLI compatibility (target subset)

| PM2 | Vexor | Notes |
|---|---|---|
| start / stop / restart / delete | same | accepts name, id, `all`, config file |
| reload | same | Implemented. Cluster mode: start new → wait healthy → retire old gracefully; a failed replacement keeps the old process and `reload` exits 1 naming the instances. Fork mode: restart by default (as PM2); `reload_strategy: start_first` for apps that can run twice (no fixed port, or `reusePort`) |
| list / ls / status | same | `--json` stable schema |
| logs | same | Implemented: `--lines`, `--follow`, `--raw`, `--err/--out`; all apps when no name, merged into one timeline by time: `HH:MM:SS.mmm app │ line` (stderr marked; JSON logs shown as level, message, fields). `--raw` prints files as written |
| describe / show | same | Implemented: env values hidden unless `--show-env`; `--json` |
| save / resurrect | same | Implemented. Vexor already restores apps from its own state whenever the daemon starts; `save` writes an explicit snapshot (`dump.json`, 0600) and `resurrect` restores from it (adds missing apps, starts those saved running, leaves differently configured apps alone) |
| startup / unstartup | same | Implemented for systemd (system unit, needs root: prints the `sudo` command otherwise) and launchd (per-user LaunchAgent). Stopping the service stops only the daemon, never the apps. OpenRC/upstart/SysV: not supported |
| scale | same | cluster mode |
| flush / reloadLogs | same | |
| monit | `vexor top` | simple TUI later |
| — | migrate --from-pm2 | Dry run, backup, apply, rollback and status implemented (docs/migration.md) |
| deploy, install, link, plus, ecosystem gen | — | unsupported, explicit error |

Install: `npm i -g vexord` (provides the `vexor` command).

Implemented so far (Phase 2.5): `start` (script, existing name, `all`, or `--spec`), `stop`, `restart`, `delete`/`del`, `list`/`ls`/`status` (`--json`). Differences from PM2 so far:
- `start` passes only an environment allowlist (PATH, HOME, USER, LOGNAME, SHELL, LANG, LC_*, TZ, TMPDIR, NODE_ENV) plus `--env KEY=VALUE`; `--inherit-env` passes the whole shell environment like PM2.
- `start` of a name that exists with a different configuration is an error instead of silently keeping the old one.
- `.js/.cjs/.mjs` run with the `node` found on the caller's PATH, resolved to an absolute path at start time.

Exit codes: 0 ok, 1 command failed, 2 usage error, 3 daemon unreachable, 4 partial success (some targets failed).

## 5. Ecosystem field classification (PM2 7.0.4 `lib/API/schema.json`, 65 fields)

Implemented in `crates/vexor-pm2` (4.1). `vexor check ecosystem.config.js` prints every finding without starting anything; `vexor start` refuses to start while any `fatal` finding remains, unless `--allow-unsupported`. Current classification (supersedes the table below where they differ):

- **fatal** (until implemented): `exec_mode: cluster` on a non-Node script, `uid`/`gid`/`username`, `increment_var`, `shutdown_with_message`, `.ts`/`.coffee` without `interpreter`, function/RegExp values, `wait_ready` on a non-Node app, malformed values.
- **partial**: `merge_logs` (instances always share log files), `log_file` (no combined log), `listen_timeout` without `wait_ready`.
- **ignored** (reported): `watch_options` (chokidar-specific), PM2 Plus/monitoring fields, `namespace`, `pid_file`, `kill_retry_time`, `force`, `log_type`, `time`, `log_date_format`, `filter_env`, `windowsHide`, unknown fields.
- `instances` without `exec_mode` runs N fork-mode processes (they can't share a port). `args`/`node_args` strings are split shell-style (quotes, backslashes) with no expansion. Environment: caller allowlist (or `--inherit-env`), then `env`, then `env_<NAME>`. Relative `cwd` resolves against the ecosystem file's directory and `script` against `cwd` (to confirm against PM2 in the conformance suite).

| Class | Fields |
|---|---|
| supported | name, script, cwd, args, interpreter/exec_interpreter, node_args/interpreter_args, env, env_*, instances, exec_mode, autorestart, max_restarts, min_uptime, restart_delay, exp_backoff_restart_delay, kill_timeout, wait_ready, listen_timeout, shutdown_with_message, stop_exit_codes, out_file, error_file, log_file, merge_logs, log_date_format, time, disable_logs, namespace, instance_var, increment_var, treekill, max_memory_restart, pid_file, autostart, append_env_to_name, kill_retry_time, filter_env, name_prefix |
| partial | watch, ignore_watch, watch_delay, watch_options (no chokidar-specific options); log_type (`json` supported, format may differ); windowsHide (Windows only); cron_restart (phase 6) |
| unsupported-fatal (unless `--allow-unsupported`) | uid, gid, username (privilege drop: phase 6, needs a security review), cron_restart (until implemented), force |
| ignored-safe (warned) | vizion, pmx, automation, trace, disable_trace, v8, event_loop_inspector, deep_monitoring, source_map_support, disable_source_map_support, install_url, post_update, execute_command, port, write, io |

"Ignored-safe" means ignoring the field doesn't change process lifecycle. It is still reported every time.

## 6. Known behavioural differences (to be confirmed by conformance tests)

- Cluster mode runs through Vexor's cluster primary on the app's own Node (not inside a Node daemon). `NODE_APP_INSTANCE` (or `instance_var`) is set per worker; `pm_id` is not set. Workers share the primary's process group, so `treekill` cleans up a worker's own child processes only when the whole app stops, not when one worker restarts. If the primary dies, all workers are restarted.
- `pm2` programmatic API (`require('pm2')`) is not provided.
- With `treekill` (default true), processes left in the app's process group after its main process **crashes** get SIGTERM, then SIGKILL after `kill_timeout`, before the restart. PM2 only kills the tree on stop/restart, so leftovers from a crash can keep ports or queue messages. `treekill: false` leaves them alone, as PM2 does.
- On stop, Vexor waits for the whole group, not just the main process, up to `kill_timeout`, then SIGKILLs what is left.
- Linux only: descendants that escape the group (`setsid`, double fork) are adopted by the shim (child subreaper), reaped when they exit, reported as `orphan` events, and stopped when the shim shuts down. **macOS:** such processes re-parent to launchd and Vexor can't track them.
- Automatic restarts are limited to one per second per instance (spawn to spawn). PM2 restarts immediately with `restart_delay: 0`.
- `listen_timeout` expiry without a ready signal is a failed start (stopped, counted as unstable, restarted with backoff). PM2 marks the app online anyway.
- `max_restarts: N` means at most N runs in a row that end before `min_uptime`, then errored, as in PM2 7 (verified by the conformance harness). `max_restarts: 0` in an ecosystem file means PM2's default, 16.
- Graceful stop sends **SIGINT** by default, as PM2 does (verified); set `kill_signal` (e.g. `"SIGTERM"`) to change it. Leftover processes after a crash still get SIGTERM, then SIGKILL.
- `max_memory_restart` (bytes, or K/M/G as in PM2): memory is checked every 5 s (PM2: every 30 s), so a leaking app is restarted sooner. As in PM2, the resident memory (RSS) of the instance's own process is measured (one worker in cluster mode), not its children, and one reading above the limit triggers a graceful restart. Each is recorded as a `memory_restart` event with the measured size. `vexor start --max-memory-restart 200M` for scripts.
- `cron_restart`: same patterns as PM2 7 (its `croner` library, via the Rust port: 5 fields, or 6 with seconds first; local time). As in PM2 (verified), every instance is restarted on schedule, and an app that exited (e.g. a job with `autorestart: false`) is run again. **Difference:** PM2 also starts an app the operator stopped (`pm2 stop`) at its next scheduled time (verified with PM2 7.1.0); in Vexor, `vexor stop` holds until `vexor start`. Each run is a `cron_restart` event. `vexor start --cron-restart "0 3 * * *"` for scripts.
- `watch` (`true` = the app's working directory, or paths relative to it), `ignore_watch`, `watch_delay`, as PM2 7 (`lib/Watcher.js`): dot-files and `node_modules` are ignored unless `ignore_watch` replaces that list; on the first change Vexor waits `watch_delay` (at least 100 ms, so one save is one restart), then restarts every instance. Differences: `ignore_watch` entries are glob patterns, matched against the path relative to the watched directory, each directory above it, and each path component (PM2 passes them to chokidar, which also takes regular expressions; regex-looking entries are reported); the app's own log files are always ignored; one watcher serves all instances (PM2 runs one per instance, so an app with N instances restarts N times per change); symbolic links are not followed. Linux uses inotify; macOS compares the tree every second (up to 100,000 entries). `vexor start --watch` for scripts.
- `health_check` (Vexor extension; PM2 has none outside PM2 Plus): `{ http: "http://127.0.0.1:3000/health" | tcp: "127.0.0.1:3000", interval: "10s", timeout: "2s", unhealthy_after: 3, restart_after: 6 }`. HTTP passes on 2xx/3xx, TCP on an accepted connection; only loopback targets (no TLS). Fork mode: each instance is checked (`{instance}` in the address is replaced by the instance number); without `wait_ready`, an instance is ready when its first check passes (within `listen_timeout`, default 30 s); after `unhealthy_after` failures in a row it is `unhealthy`, after `restart_after` it is restarted. Cluster mode: workers share the port, so the app is checked as a whole; after `restart_after` failures it is reloaded (new workers first), counting only once the app has passed a check, so a check that never passes doesn't cause endless reloads. Changes between passing and failing are `health` events. `vexor start --health-check URL|ADDR`.
- Log timestamps: Vexor starts every plain-text log line with the local time it was received (`2026-10-23T15:17:05.344+05:30 `), so `vexor logs` can merge apps into one timeline; JSON lines (starting with `{`) are left as written. PM2 writes timestamps only with `time: true` or `log_date_format`; in Vexor `time: false` turns them off, and `log_date_format` is noted (Vexor's format is fixed).
- `@pm2/io` metrics/actions are not collected.
- Log rotation is built in and on by default: 10 MiB per file, 5 rotated files (PM2 needs `pm2-logrotate`, whose defaults are 10 MB and 30 files).
- When an app writes output faster than the disk can take it, Vexor discards the excess by default and says so in the log and in a `log_dropped` event, so the app never waits on logging. `logs.overflow: block` makes the app wait instead (nothing lost).
- `describe` redacts env values.

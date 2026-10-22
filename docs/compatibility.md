# Vexor Compatibility Specification (Phase 1 draft)

Three separate questions, always answered separately:

1. **Runtime compatibility**: can Vexor launch and supervise the app?
2. **Behavioural compatibility**: does Vexor reproduce the PM2 behaviour the app relies on?
3. **Security support**: does the runtime still get upstream fixes? (Vexor warns on EOL Node and never forces an upgrade.)

Status values: `planned` (spec only), `tested` (green in CI), `qualified` (works with stated limits), `unsupported`.
Every row below is `planned` until CI says otherwise.

Manual spot checks so far (2026-10-22, not yet CI): the static linux-arm64 musl binary on Alpine with Node 8.17, 12.22, 16.20 and 24.22 handled `process.send('ready')`, natural exit, and graceful SIGTERM correctly. The full shim suite passed on Linux arm64 (Debian bookworm, Node 18.20) and macOS arm64 (Node 24.22).

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
| Node | ≥ 10 (tested 10, 12, 14, 16, 18, 20, 22, 24) |
| npm | ≥ 6 (optional deps with `os`/`cpu` filtering) |
| Package managers | npm, pnpm, yarn classic; yarn berry PnP = `qualified` |
| Below the floor | direct binary install: release tarball + SHA-256 + `install.sh` |

The wrapper is a zero-dependency ES5 script with no `postinstall` and no network access.

## 4. CLI compatibility (target subset)

| PM2 | Vexor | Notes |
|---|---|---|
| start / stop / restart / delete | same | accepts name, id, `all`, config file |
| reload | same | start new → wait healthy → retire old; failed replacement keeps old running. Fork mode on a fixed port **falls back to restart and says so** (architecture.md §6a) |
| list / ls / status | same | `--json` stable schema |
| logs | same | `--lines`, `--raw`, `--err/--out` |
| describe / show | same | env values redacted by default |
| save / resurrect | same | |
| startup / unstartup | same | systemd/launchd only |
| scale | same | cluster mode |
| flush / reloadLogs | same | |
| monit | `vexor top` | simple TUI later |
| deploy, install, link, plus, ecosystem gen | — | unsupported, explicit error |

Install: `npm i -g vexord` (provides the `vexor` command).

Implemented so far (Phase 2.5): `start` (script, existing name, `all`, or `--spec`), `stop`, `restart`, `delete`/`del`, `list`/`ls`/`status` (`--json`). Differences from PM2 so far:
- `start` passes only an environment allowlist (PATH, HOME, USER, LOGNAME, SHELL, LANG, LC_*, TZ, TMPDIR, NODE_ENV) plus `--env KEY=VALUE`; `--inherit-env` passes the whole shell environment like PM2.
- `start` of a name that exists with a different configuration is an error instead of silently keeping the old one.
- `.js/.cjs/.mjs` run with the `node` found on the caller's PATH, resolved to an absolute path at start time.

Exit codes: 0 ok, 1 command failed, 2 usage error, 3 daemon unreachable, 4 partial success (some targets failed).

## 5. Ecosystem field classification (PM2 7.0.4 `lib/API/schema.json`, 65 fields)

| Class | Fields |
|---|---|
| supported | name, script, cwd, args, interpreter/exec_interpreter, node_args/interpreter_args, env, env_*, instances, exec_mode, autorestart, max_restarts, min_uptime, restart_delay, exp_backoff_restart_delay, kill_timeout, wait_ready, listen_timeout, shutdown_with_message, stop_exit_codes, out_file, error_file, log_file, merge_logs, log_date_format, time, disable_logs, namespace, instance_var, increment_var, treekill, max_memory_restart, pid_file, autostart, append_env_to_name, kill_retry_time, filter_env, name_prefix |
| partial | watch, ignore_watch, watch_delay, watch_options (no chokidar-specific options); log_type (`json` supported, format may differ); windowsHide (Windows only); cron_restart (phase 6) |
| unsupported-fatal (unless `--allow-unsupported`) | uid, gid, username (privilege drop: phase 6, needs a security review), cron_restart (until implemented), force |
| ignored-safe (warned) | vizion, pmx, automation, trace, disable_trace, v8, event_loop_inspector, deep_monitoring, source_map_support, disable_source_map_support, install_url, post_update, execute_command, port, write, io |

"Ignored-safe" means ignoring the field doesn't change process lifecycle. It is still reported every time.

## 6. Known behavioural differences (to be confirmed by conformance tests)

- Cluster mode runs through `cluster-shim.js` on the app's Node, not inside the daemon. `process.env.NODE_APP_INSTANCE` and `pm_id` are set the same way.
- `pm2` programmatic API (`require('pm2')`) is not provided.
- With `treekill` (default true), processes left in the app's process group after its main process **crashes** get SIGTERM, then SIGKILL after `kill_timeout`, before the restart. PM2 only kills the tree on stop/restart, so leftovers from a crash can keep ports or queue messages. `treekill: false` leaves them alone, as PM2 does.
- On stop, Vexor waits for the whole group, not just the main process, up to `kill_timeout`, then SIGKILLs what is left.
- Linux only: descendants that escape the group (`setsid`, double fork) are adopted by the shim (child subreaper), reaped when they exit, reported as `orphan` events, and stopped when the shim shuts down. **macOS:** such processes re-parent to launchd and Vexor can't track them.
- Automatic restarts are limited to one per second per instance (spawn to spawn). PM2 restarts immediately with `restart_delay: 0`.
- `listen_timeout` expiry without a ready signal is a failed start (stopped, counted as unstable, restarted with backoff). PM2 marks the app online anyway.
- `max_restarts: N` allows N automatic restarts of consecutive unstable runs; the (N+1)th unstable exit marks the app errored. Exact PM2 off-by-one to be confirmed in conformance tests.
- `@pm2/io` metrics/actions are not collected.
- Log rotation is built in and on by default (PM2 needs `pm2-logrotate`).
- `describe` redacts env values.

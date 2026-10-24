# Changelog

All notable changes to Vexor. Versions follow [Semantic Versioning](https://semver.org/);
before 1.0, minor versions may include breaking changes, which are listed here.

## Unreleased

### Added
- `vexor monit`: a live view of every app (PM2 `monit`): CPU and memory
  bars, restarts, uptime and health per instance, the open alerts, and
  the latest log lines of all apps, redrawn every second.
- `vexor scale <app> <N|+N|-N>` (PM2 `scale`): only the difference is
  started or stopped; running instances are untouched. The new count is
  kept across restarts.
- Starting an existing app with only a different instance count (a
  changed `instances` in the ecosystem file, or `-i`) scales it instead
  of reporting a conflict.

### Changed
- Shim protocol 3 (scaling). A daemon refused by an older shim steps
  down one version at a time, so shims from 0.4–0.8 keep their metrics;
  scaling an app supervised by a pre-0.9 shim asks for one restart first.

## 0.8.0 (2026-10-24, prerelease)

### Added
- PM2 commands: `vexor flush [app]` empties log files, `vexor signal
  <SIG> <app>` (alias `send-signal`) signals every running instance
  (identity-checked, so a reused PID is never hit), and `vexor kill`
  stops every app and then the daemon.
- PM2 `increment_var`: each instance gets the variable's value plus its
  index (`PORT=3000` gives 3000, 3001, ...), in fork and cluster mode.
  Ecosystem files using it no longer stop a migration.
- PM2 `shutdown_with_message`: stopping, restarting or reloading sends the
  app the message `'shutdown'` (fork and cluster mode) instead of a
  signal; SIGKILL still follows after `kill_timeout`.
- Prometheus endpoint: `{"prometheus": {"listen": "127.0.0.1:9615"}}` in
  `settings.json` serves per-instance up/state, restarts, uptime, CPU,
  memory, open files and health, plus open alerts, at `/metrics`. Off by
  default; warned about when it listens beyond loopback. See
  docs/prometheus.md.

## 0.7.0 (2026-10-24, prerelease)

### Fixed
- Log lines were dropped when an app wrote many short lines while the
  disk was briefly slow: the 1 MiB log buffer was counted in reads (128
  of them), so a few kilobytes of short lines filled it. It now holds
  1 MiB of output however it is split, and memory stays bounded.

### Security
- Webhook URLs are treated as secrets (Slack's carries its token): only
  the scheme and host are ever shown or logged.
- Email header values are stripped of line breaks and control characters.
- An SMTP server can't hold a delivery open: replies are capped at 100
  lines and a whole delivery at 60 s.

### Added
- Alert delivery by email (any SMTP server; STARTTLS, implicit TLS,
  AUTH PLAIN/LOGIN) and webhooks (JSON signed with HMAC-SHA256, or Slack
  format), with retries and per-level filters. `vexor alerts test` sends
  a test to every channel. Secrets can live in files and are never shown.
  See docs/alerts.md.

### Changed
- The release binary grows from 3.4 MB to 4.2 MB (TLS for email and
  webhooks, without OpenSSL).

## 0.6.0 (2026-10-24, prerelease)

### Fixed
- Resource history lost the last sample of a process that crashed when
  its replacement was sampled within the same second (samples are now
  stored to the millisecond; existing history is migrated).

### Added
- Alerts: incidents for crash loops, frequent restarts, failing health
  checks and log files filling the disk, with no configuration; CPU,
  memory and open-file thresholds in `alerts.json`. Levels must hold for
  a while before an incident opens, it notifies once per level and when
  it recovers, and flapping incidents go quiet. `vexor alerts` shows open
  incidents, the last week of alerts and the rules. See docs/alerts.md.

## 0.5.0 (2026-10-24, prerelease)

### Added
- Resource history: `vexor metrics <app> [--since 1h] [--json]` shows CPU
  and memory sparklines, averages, peaks, open files and restarts per
  instance. Samples are kept for 24 hours, per-minute averages for 7 days
  and per-hour averages for 90 days, in about 0.85 MiB per process.

### Fixed
- `vexor logs --follow` skipped a whole file when a log rotated twice
  between two reads (a fast writer with small `max_size`).

## 0.4.0 (2026-10-24, prerelease)

### Added
- Live resource metrics: each shim samples its instances every 5 s
  (`metrics_interval_ms`, 0 = off): CPU over the interval, memory, open
  files and the latest health result. `vexor describe` shows them in a new
  Resources section; `list --json` and `describe --json` include them.
  Sampling 100 apps costs under 1% of one core.

### Changed
- `vexor ls` CPU is the load over the last 5 s. Before, on Linux it was the
  average over the process's whole life, so an app that had been running
  for a while showed close to 0% even when busy.
- Shim protocol 2. Shims started by an older Vexor keep working after an
  upgrade (without metrics; `ls` falls back to `ps` for them).

## 0.3.1 (2026-10-23, prerelease)

Benchmarked against PM2, OxMgr and forever (results and charts in the
README and docs/benchmarks.md), with the weak points they showed fixed.

### Performance
- Starting the apps of an ecosystem file is parallel: 10 apps in 352 ms
  (was 2.4 s; PM2: 435 ms).
- Idle CPU with 50 apps is 0.33% of a core (was 6.6%; PM2: 1.5%): each
  shim reads only its own children instead of scanning /proc.

### Fixed
- A daemon left running by another Vexor version (e.g. after an upgrade)
  refused new settings ("unknown field `timestamps`"), so a migration
  failed. The CLI now asks the daemon its version and restarts a
  different one gracefully; apps keep running.
- When Vexor refused an app during `migrate --apply`, the app was left
  stopped under PM2. It is now always started again under PM2.
- The migration dry run flags apps Vexor would refuse (e.g. names longer
  than 64 characters) as blocked.

### Changed
- `vexor ls` shows app names without colour.

## 0.3.0 (2026-10-23, prerelease)

A modern CLI (colours, tables, one log timeline with timestamps), HTTP/TCP
health checks, and a PM2 migration that works on a real 13-app setup.
Still a prerelease: no published benchmarks yet, and `uid`/`gid`,
`increment_var` and `shutdown_with_message` are not supported.

### Added
- HTTP/TCP health checks (`health_check` in ecosystem files, `vexor start
  --health-check`): an instance is ready when its first check passes and
  is restarted after repeated failures; cluster apps are reloaded instead.
  Loopback targets only.

- Log lines are timestamped: Vexor starts each plain-text line with the
  local time it was received (ISO 8601 with milliseconds and UTC offset);
  JSON lines keep their own time. `time: false` turns it off per app.
- Colours and tables: `vexor ls` (status badges, CPU, memory, instances
  grouped per app), `describe`, `events`, `migrate` and
  `migrate --status`; `--color auto|always|never` (and `NO_COLOR`). Piped
  output stays plain and aligned. `vexor` alone shows the common commands.

### Changed
- `vexor logs` shows one timeline: lines from every app merged by time,
  each with its time, the app name in its own colour and a stream marker
  (stderr in red); JSON logs (pino, winston) are shown as level, message
  and fields. `--raw` prints the files as written.
- `vexor migrate --from-pm2` shows only the plan and what needs attention
  (`--verbose` for commands, paths and environment). An app PM2 was
  already crash-looping no longer stops a migration; a slow-starting shim
  no longer makes a start fail.
- `vexor migrate --from-pm2` report: findings common to all apps are said
  once; environments are summarised (shared set once, per-app
  differences); the table gives the blocking reason; apps PM2 keeps
  restarting are flagged. `--env-names` lists every variable name.

## 0.2.0 (2026-10-23, prerelease)

Moving from PM2: a migration that plans, backs up, moves apps one at a
time with automatic fallback, and rolls back; and the three PM2 settings
that used to block apps (memory, cron and watch restarts). Still a
prerelease: no published benchmarks yet, and `uid`/`gid`,
`increment_var` and `shutdown_with_message` are not supported.

### Added
- `max_memory_restart` (ecosystem files, `vexor start --max-memory-restart
  200M`): an instance whose resident memory goes above the limit is
  restarted gracefully; checked every 5 s and recorded as a
  `memory_restart` event.
- `cron_restart` (ecosystem files, `vexor start --cron-restart "0 3 * * *"`):
  restarts every instance on a schedule, with PM2 7's pattern syntax; an
  app that exited is run again. Unlike PM2, an app stopped with
  `vexor stop` stays stopped.
- `watch`, `ignore_watch`, `watch_delay` (ecosystem files, `vexor start
  --watch`): restart every instance when files change; PM2's default
  ignore list; the app's own logs are never a trigger. inotify on Linux,
  polling every second on macOS.
- `vexor migrate --from-pm2 --dry-run`: reads PM2's `dump.pm2` and shows
  how each app would run under Vexor, every unsupported setting, and the
  expected downtime. Changes nothing (no PM2 command, no Vexor state).
- `vexor migrate --from-pm2 --backup`: copies PM2's state, its boot service
  file and the migration plan to a private, never-overwritten directory
  under `~/.vexor/migrations/`, with SHA-256 hashes.
- `vexor migrate --from-pm2 --apply`: moves apps one at a time (`pm2 stop`,
  start under Vexor, wait until healthy), puts an unhealthy app back under
  PM2 and stops, journals every step so a rerun resumes, and only applies
  a reviewed plan whose PM2 dump hasn't changed since the backup.
- `vexor migrate --rollback [ID]`: moves migrated apps back to PM2, newest
  first, asking before each; `vexor migrate --status` shows where each app
  stands.

## 0.1.1 (2026-10-23, prerelease)

Fixes found by running the same ecosystem files under PM2 7.1.0 and Vexor
(`tests/conformance`).

### Changed (PM2 compatibility)
- **Stop signal is now SIGINT**, as PM2 (was SIGTERM). Apps that only
  handle SIGINT now shut down gracefully instead of being killed. New
  `kill_signal` setting (also in ecosystem files) to choose another signal.
- **`max_restarts: N` now allows N runs in a row** before errored, as PM2
  (0.1.0 allowed N + 1). `max_restarts: 0` in an ecosystem file means PM2's
  default, 16.

### Added
- PM2 conformance harness (`tests/conformance`) and report
  (`docs/conformance.md`): 11 scenarios, 10 identical to PM2 7.1.0, 1
  intentional difference.

### Fixed
- `install.sh` with no version resolved GitHub's "latest" release, which
  skips prereleases, and failed with a 404; it now installs the newest
  release.

## 0.1.0 (2026-10-22, prerelease)

First public prerelease. **Not yet recommended for production**: there is no
PM2 migration tool, no memory/watch/cron restarts, and no published
benchmarks yet (see `docs/plan.md`).

### Supervision
- Per-app shim keeps apps and logs running while the daemon crashes,
  restarts or is upgraded (tested: 100 daemon kills under HTTP load, 0 failed
  requests).
- Restart with fixed or exponential backoff and jitter, crash-loop limit
  (`max_restarts` / `min_uptime`), at most one automatic restart per second
  per instance.
- Readiness via `process.send('ready')` (`wait_ready` / `listen_timeout`);
  a missing ready signal is a failed start, not "online".
- Graceful stop of the whole process group, SIGKILL after `kill_timeout`;
  leftover and escaped child processes are cleaned up (escaped ones on
  Linux).
- Process identity is PID plus start time, so a reused PID is never
  signalled.

### PM2 compatibility
- `ecosystem.config.{js,cjs,mjs,json}`, evaluated with the app's own Node.
  Every field is classified; unsupported fields that would change behaviour
  stop the start unless `--allow-unsupported` (`vexor check` shows them).
- Cluster mode (`exec_mode: cluster`, `vexor start app.js -i N`).
- `reload`: zero-downtime in cluster mode (new worker healthy before the
  old one retires; a failed replacement keeps the old one).
- Commands: `start`, `stop`, `restart`, `reload`, `delete`, `list`,
  `describe`, `logs`, `events`, `save`, `resurrect`, `startup`,
  `unstartup`, `check`.

### Operations
- Desired state in `state.json`; event history in SQLite (`vexor events`).
- Log rotation built in (10 MiB × 5 by default); output floods can't block
  the app (`overflow: drop`, counted and reported) unless `block` is chosen.
- `vexor startup` for systemd (system unit) and launchd (LaunchAgent);
  stopping the service never stops apps.

### Runtimes and platforms
- Managed apps: tested on Node.js 8, 10, 12, 14, 16, 18, 20, 22 and 24.
- npm launcher (`npm i -g vexord`): Node.js 10 or newer; or `install.sh`
  without Node.
- Binaries: Linux x64 and arm64 (static, any distribution), macOS arm64.
  macOS x64 is built but not yet tested on Intel hardware.

### Differences from PM2 worth knowing
See `docs/compatibility.md`. Highlights: only an allowlist of the caller's
environment is passed unless `--inherit-env`; `start` of an existing name
with a different configuration is an error; automatic restarts are limited
to one per second per instance.

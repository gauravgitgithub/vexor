# Changelog

All notable changes to Vexor. Versions follow [Semantic Versioning](https://semver.org/);
before 1.0, minor versions may include breaking changes, which are listed here.

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

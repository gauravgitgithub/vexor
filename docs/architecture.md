# Vexor Architecture (Phase 1 draft)

Status: proposal for review. Nothing here is implemented yet. Decisions marked
**OPEN** need sign-off before Phase 2 starts.

## 1. Runtime layers

| Layer | What | Depends on Node? | Compatibility promise |
|---|---|---|---|
| A. Supervisor + CLI | One static Rust binary `vexor` (`vexor daemon` runs the supervisor) | No | Defined by OS/arch/kernel only |
| B. npm wrapper | `vexord` npm package (installs the `vexor` command): ES5 bin shim + per-platform optional packages | Yes (wrapper only) | Its own Node/npm matrix (see compatibility.md) |
| C. Managed app | Whatever executable the app config selects | The app's own Node, if any | Per-runtime test matrix |

Rules:
- Layer A never assumes `node` is on `PATH`. The interpreter is resolved at
  `start` time from the CLI caller's environment, or from the config. The
  resolved absolute path is stored in desired state, so the daemon's own `PATH`
  doesn't matter later.
- Layer B never runs in the supervision path. If the wrapper can't run on a
  legacy host, users install the binary directly (tarball + checksum, or
  `curl | sh` with checksum verification). Behaviour is identical.
- Layer A ships two small JavaScript helpers that run on the **app's** Node
  (Layer C), never on the wrapper's Node:
  1. `ecosystem-loader.js`: `require()`s an `ecosystem.config.js` and prints JSON.
  2. `cluster-shim.js`: PM2-compatible cluster master (see §6).
  Both are written in ES5 (no `const`/arrow/template/optional chaining) and are
  embedded in the binary. They are tested on every Node version in the matrix.

## 2. Process topology

```
vexor (CLI) ──unix socket (0600, peer-cred)──▶ vexor daemon  (control plane: desired state,
                                                │               IPC, events.db, metrics)
                                                │ per-app socket $VEXOR_HOME/shims/<app>.sock (0600)
                     ┌──────────────────────────┼──────────────────────────┐
                     ▼                          ▼                          ▼
              vexor shim (app A)          vexor shim (app B)         vexor shim (app C)
              data plane: holds pipes,    │                          │
              writes+rotates logs,        ▼                          ▼
              restart policy, NODE_       cluster-shim.js (app's     non-Node exe
              CHANNEL_FD, subreaper       node) └─ cluster.fork()×N
              ▼
              fork-mode app × instances (own pgid each)
```

- The daemon never parents app processes. One **`vexor shim`** per app (the
  same binary run as a `shim` subcommand) is the parent of that app's instances
  and owns their pipes, log files and restart loop. The shim runs in its own
  session, so it survives a daemon crash, `kill -9`, or upgrade (see §7).
- Each managed process gets its own process group (`setsid`), so a stop
  signal reaches the whole tree via `kill(-pgid)`.
- On Linux both shim and daemon are child subreapers. Grandchildren that
  daemonize re-parent to the shim, not PID 1. If a shim dies, its children
  re-parent to the daemon, which then adopts them (identity-checked).
- **NODE_CHANNEL_FD**: for Node apps the shim creates a socketpair and sets
  `NODE_CHANNEL_FD` (JSON serialization). Node then gives the app a working
  `process.send`. That makes PM2's `wait_ready` (`process.send('ready')`) and
  `shutdown_with_message` work without a Node master process. Verified
  manually on Node 8, 12, 16 and 24 (`tests/legacy/node-smoke.sh`). An
  open channel does not keep the app alive unless the app listens for
  messages, same as under PM2.

## 3. Crate layout (minimal; split only when a boundary pays for itself)

```
crates/
  vexor-core/    pure logic, no I/O: config model, lifecycle state machine,
                 restart policy/backoff, validation, redaction. Heavily unit-tested.
  vexor-proto/   IPC request/response types + framing, versioned.
  vexor-daemon/  control plane: desired state, reconcile, IPC server, events.db, metrics.
  vexor-shim/    data plane: spawn, identity, signals, pipes, logs, restart loop.
  vexor-pm2/     PM2 ecosystem/dump parsing, field compatibility table, migration planner.
  vexor/         the binary: clap CLI + `daemon` and `shim` subcommands.
npm/
  vexord/                   ES5 wrapper, zero dependencies; bin: `vexor`
  platforms/<os>-<arch>/    `vexord-<os>-<arch>`, one per target, contains the binary
```

Metrics, logging and state stay modules inside `vexor-daemon` until they need a
separate crate.

## 4. Lifecycle state machine (in `vexor-core`, single owner)

States: `stopped`, `starting`, `online`, `unhealthy`, `stopping`, `backoff`,
`errored` (crash-loop budget exhausted), `unknown` (after a daemon restart,
before identity is checked).

| From | Event | To |
|---|---|---|
| stopped / errored | start | starting |
| starting | spawned + (no readiness configured) | online |
| starting | ready signal / health pass | online |
| starting | readiness timeout | stopping → backoff |
| online | health fail × N | unhealthy |
| unhealthy | health pass | online |
| unhealthy | unhealthy timeout | stopping → backoff |
| online / starting / unhealthy | exit (autorestart, budget left) | backoff |
| backoff | timer fires | starting |
| any running | exit, budget exhausted | errored |
| any running | stop | stopping → stopped (SIGTERM → kill_timeout → SIGKILL to pgid) |
| unknown | identity verified alive | online (adopted, no log pipes) or stopping (policy) |
| unknown | identity mismatch / dead | backoff or stopped (per desired state) |

Implemented in `crates/vexor-core` (`Instance::handle(event, ctx) -> Vec<Effect>`).
The shim runs the effects. Timers carry a generation number, so a timer that
fires after it was replaced or cancelled is ignored. Every `(state, event)`
pair is handled; events that make no sense in a state are ignored with no
effects.

Restart policy:
- Delay: `restart_delay` (fixed) or `exp_backoff_restart_delay` (×1.5 per
  attempt, capped, reset after a run of ≥ 30 s). Optional equal jitter: uniform
  in `[delay/2, delay]`.
- **Rate floor:** automatic spawns of one instance are at least 1 s apart,
  spawn to spawn, whatever the config (`MIN_RESTART_INTERVAL`). Operator
  commands aren't limited.
- **Crash budget:** a run shorter than `min_uptime`, or one that never became
  ready, is *unstable*. After `max_restarts` consecutive unstable runs have
  been restarted, the next unstable exit marks the instance `errored`. A stable
  run resets the count; so do operator `start`/`restart`.
- `autorestart: false` or an exit code in `stop_exit_codes` → `stopped`.

Verified by tests: every event in every state keeps the invariants; arbitrary
event sequences (property-tested) never break them; automatic spawns never
violate the 1 s floor; a crash loop always ends in `errored` after exactly
`max_restarts` restarts. Mutation checks confirmed the tests fail when the
floor or the budget is broken.

## 5. Process identity

A PID alone is never trusted. Identity = `(pid, start_time, exe_path)`:
- Linux: start time from `/proc/<pid>/stat` field 22. Signal through `pidfd`
  (kernel ≥ 5.3, race-free). Older kernels fall back to re-checking start time
  right before `kill`.
- macOS: `proc_pidinfo(PROC_PIDTBSDINFO)` → `pbi_start_tvsec/usec`.
- Windows (later): process handle + creation time; Job Object for tree kill.

Identity is persisted with desired state. After a daemon restart, a recorded
PID whose start time doesn't match is treated as **not ours**. Vexor never
signals it.

## 6. Cluster mode

PM2 cluster mode works because the PM2 daemon *is* a Node `cluster` primary, so
workers share listening sockets. A Rust daemon can't play that role. Options:

| Option | App changes | Legacy Node | Port sharing semantics |
|---|---|---|---|
| **A. cluster-shim** (recommended): Vexor runs `app-node cluster-shim.js`, which `cluster.fork()`s N workers and reports events over NODE_CHANNEL_FD | None | Yes (cluster API since 0.6; shim uses `setupMaster` fallback) | Identical to PM2 |
| B. SO_REUSEPORT, N independent forks | `reusePort: true` (Node ≥ 22.12/23.1) | No | Kernel load-balancing, Linux only |
| C. Daemon-held socket passed by fd | App must listen on fd | Partial | Different |

With option A, Vexor supervises each worker's lifecycle through the shim. The
shim is the only Node code in the supervision path. It is ~150 lines of ES5,
and Vexor restarts it if it dies. Rolling reload = shim forks a replacement,
waits for `listening`/`ready`, then disconnects the old worker.

## 6a. Reload: start new → wait healthy → retire old

Every reload runs per instance (one at a time, or `reload_batch` at a time):

1. Start the replacement next to the old instance (old keeps serving).
2. Wait until the replacement is **healthy**, in priority order:
   `process.send('ready')` (wait_ready) → HTTP/TCP health check → in cluster
   mode, the worker's `listening` event → alive for `min_uptime` (weakest; a
   warning says so).
3. Healthy within `listen_timeout` → send the old instance its shutdown
   (`shutdown_with_message` or SIGTERM), wait for connections to drain up to
   `kill_timeout`, then SIGKILL its process group.
4. Not healthy in time, or exits early → kill the **replacement**, keep the old
   instance, mark the reload `failed` with the reason. A failed reload never
   costs availability. (This fixes the PM2 behaviour where `wait_ready` marks
   failed launches online: #5013, #5716.)

Whether the old and new instances can overlap depends on the port:

| Topology | Overlap possible? | Strategy |
|---|---|---|
| Cluster mode, **any instance count including 1** | yes: the cluster-shim primary holds the listening socket, so new and old workers share it | `start-first` always |
| Fork mode, app doesn't listen (queue workers, consumers) | yes, but two copies run briefly | `start-first` if `reload_strategy: start-first`; else `stop-first` |
| Fork mode with `reusePort: true` (Node ≥ 22.12/23.1, Linux) | yes | `start-first` (opt-in) |
| Fork mode on a fixed port, no `reusePort` | **no**: new process gets `EADDRINUSE` | `stop-first` (gap) |

`reload_strategy`: `auto` (default) | `start-first` | `stop-first`.
- `auto` = `start-first` in cluster mode, `stop-first` in fork mode. The fork
  default matches PM2 and avoids running two copies of singleton workers (cron
  jobs, leader-only consumers) without the user asking.
- `start-first` in fork mode on a fixed port fails safely: the replacement dies
  with `EADDRINUSE`, the old one keeps running, and the error says to use
  cluster mode (`-i 1` is enough) or `reusePort`.
- Every reload reports which strategy ran, and says "restart, not zero
  downtime" when it was `stop-first`.

Implemented (plan 4.3) as a phase of the core state machine (`Event::Reload`,
`Replacement*` events, `SpawnReplacement` / `PromoteReplacement` /
`AbandonReplacement` effects, `Timer::Replacement`). Without
`ready_timeout`, cluster reload waits for `listening` up to
`reload_timeout_ms` (default 30 s); fork start-first waits until the
replacement has been up for `min_uptime` (at least 1 s). Retired cluster
workers are disconnected first (no new connections, in-flight ones finish),
then sent SIGTERM, then SIGKILL after `kill_timeout`.

**For a single-instance HTTP app, the zero-downtime path is cluster mode with
one instance.** `vexor migrate --dry-run` and `doctor` suggest it for fork-mode
apps that listen on a port.

## 7. Daemon crash / upgrade recovery — **Decided: per-app shim (R2)**

Goal: a daemon crash, `kill -9`, OOM kill, or version upgrade causes **zero
app downtime and zero log loss**. Rejected alternatives: R1 (adopt orphans;
apps survive but logs stop) and R3 (apps die with the daemon).

How it works:
- The shim holds everything the app needs at runtime: child processes, pipes,
  log writer + rotation, NODE_CHANNEL_FD, and the restart policy (the same
  `vexor-core` code). While the daemon is gone, crashed instances are still
  restarted with backoff and logs keep flowing.
- Shim ↔ daemon talk over a per-app 0600 socket with a versioned protocol.
  While disconnected, the shim buffers events in a bounded queue (overflow is
  counted, not silent) and replays them on reconnect.
- Daemon startup: read `state.json`, scan `$VEXOR_HOME/shims/`, connect to each
  shim, verify the shim's and children's identity (pid + start time), and
  reconcile. A shim found running is never restarted. A desired app with no live
  shim gets a new one.
- Daemon upgrade: stop the old daemon, start the new one. Apps aren't touched.
- Shim upgrade (Phase 6): the shim `execve`s the new binary in place. The PID and
  child relationships are preserved by exec, and state plus fds are handed over
  through an inherited fd, so apps aren't restarted. Until that lands, a version
  skew between shim and daemon is supported for N-1 protocol versions and
  flagged by `doctor`. Implemented: protocol 2 added metrics, 3 added scaling,
  4 added per-route stats;
  shims accept every version from 1, and a daemon refused by an older shim
  says hello again one version lower until it is accepted.
- If a shim itself dies (it is small and does little), its children re-parent
  to the daemon (subreaper), which adopts them identity-checked and starts a
  replacement shim at the app's next restart. Logs stop for those children
  until then, and that is reported. This is the R1 path, used only as a fallback.
- One shim per **app**, not per instance: `-i 8` is still one shim.

Cost: one extra process per app. Text pages of the shared binary are shared
across processes, so per-shim private RSS is expected to be well under 1 MB
(unverified; measured in B3). Benchmarks count shims as supervisor overhead.

### Implemented (plan step 2.4b)

- Files under `$VEXOR_HOME/shims/`: `<app>.lock` (flock held for the shim's
  life: one shim per app, and "lock free" means "no shim"), `<app>.sock`
  (control socket, 0600, peer UID checked on both sides), `<app>.pids.json`
  (instance leader identities), `<app>.log` (shim's own output).
- Protocol (`crates/vexor-proto`): JSON lines. Daemon sends `hello` with an
  optional `resume {shim_start, last_seq}`; shim answers `welcome` (identity,
  next sequence number, latest state per instance), replays buffered events
  if resuming the same shim incarnation, then streams. The buffer holds
  10,000 events; anything older is reported as `gap`, never silently skipped.
- The daemon (`vexor daemon --home DIR`) takes `daemon.lock`, then for each
  `apps/*.json` connects to the shim or launches it (own process group,
  output to the shim log). If no shim holds the lock but its recorded
  instances are still alive (a shim crash), it does **not** launch a new
  shim: it logs `orphaned_instances` and waits. The adoption fallback
  described above is not implemented yet.
- Verified: HTTP app under back-to-back requests from one client while the
  daemon is `kill -9`ed and restarted 100 times: 0 failed requests, same app
  PID, one shim launched, every request logged exactly once in order
  (macOS arm64 ×5, Linux arm64 ×3). A crashed app is restarted by the shim
  while the daemon is down. A mutant where the shim stops its apps when the
  daemon disconnects fails these tests.

Not yet: resume across daemon restarts (needs `events.db`, step 2.6b; a fresh
daemon gets the current snapshot, not missed history), operator commands
(step 2.5), desired state so a deliberately stopped shim is not relaunched
(2.6), in-place shim upgrade, and adopting a dead shim's instances.

### What zero downtime does and does not cover

| Event | App downtime | Why |
|---|---|---|
| Daemon crash / kill -9 / OOM / upgrade | **none** (target, proven by fault-injection tests) | shims keep apps and logs running |
| `vexor reload`, cluster mode (any instance count, including 1) | **none** (target) | start new → healthy → retire old (§6a) |
| `vexor reload`, fork mode, `start-first` (non-listening worker or `reusePort`) | **none** (target) | as above; two copies overlap briefly |
| `vexor reload`, fork mode on fixed port | **gap** | port can't be shared; reported as restart. Fix: cluster mode `-i 1` |
| Failed reload (replacement unhealthy) | **none** | old instance kept running |
| App crash | gap until restart (single instance) or none for other instances | inherent |
| Host reboot | gap | inherent |
| PM2 → Vexor migration | **brief gap per app** | PM2 holds the listening socket; Vexor can't share it. Zero downtime needs ≥ 2 hosts behind a load balancer, or an app using `SO_REUSEPORT` (Node ≥ 22.12/23.1, Linux) |

## 8. Durable state

**Decided: hybrid.** Desired state and event history have different shapes,
so each gets its own store.

| Store | Holds | Write pattern | Authority |
|---|---|---|---|
| `state.json` | app definitions, resolved config, identity records | rare (user commands), whole-document rewrite | **authoritative** for what should run |
| `events.db` (SQLite, rusqlite bundled, WAL) | lifecycle events (start/exit/restart with code, signal, uptime), restart-budget window, operation log, migration journal, process metrics with rollups (plan Phase 6b) | frequent, small appends | history only |

`state.json`: written via temp file + `fsync` + `rename` + dir `fsync`, mode
0600, with a schema version and checksum. It stays human-readable and
hand-editable during incidents. Unknown fields get serde defaults; a newer
schema version is refused rather than downgraded.

`events.db`:
- Mode 0600. Schema version in `PRAGMA user_version`, with forward-only
  migrations run at daemon start inside a transaction.
- Owned by one dedicated writer thread fed by a bounded channel. Lifecycle code
  never waits on SQLite. If the channel is full, events are counted as dropped
  and the count is reported, not hidden.
- Crash-loop budgets are enforced by the shim, which survives daemon
  restarts (§7), so they don't depend on this database. Its jobs are
  history, resuming the shim event stream, and later metrics (Phase 6b).
- Every shim event is stored together with the app's resume cursor
  (`shim_start`, `last_seq`) in one transaction. A restarted daemon resumes
  from the cursor; without one it asks for the whole shim buffer, and
  `UNIQUE (app, shim_start, seq)` drops anything already stored.
- Retention: prune by age (default 30 days) and row cap (default 1M) on a timer.
- `vexor events [app] [--since 2h] [--limit N] [--json]` queries it.
  `PRAGMA quick_check` runs at every daemon start.

Failure rules:
- Corrupt or newer-schema `state.json` → the daemon refuses to start, keeps the
  file, and prints a recovery hint (`vexor doctor`). It never truncates.
- `events.db` corrupt → renamed `events.db.corrupt-<ts>` (never deleted), a
  fresh one is created, and CLI responses carry a warning for the rest of the
  daemon's life. If no database can be opened at all, supervision continues
  without history and the warning says so.
- `doctor` warns when `$VEXOR_HOME` is on a network filesystem, because SQLite
  WAL is unsafe there.

Cost accepted: about 1 MB more binary and a C toolchain in the cross-build
(musl targets included).

## 9. IPC

- Unix socket `$VEXOR_HOME/daemon.sock`. `$VEXOR_HOME` (default
  `~/.vexor`) is created 0700; the socket is 0600. The peer UID is checked via
  `SO_PEERCRED` / `getpeereid` and must equal the daemon UID (root is allowed).
- Framing: JSON lines (same codec as daemon ↔ shim), max 1 MiB per line,
  one request per connection. Each request carries the protocol version; a
  mismatch gives an actionable error.
- Mutating commands are serialized by one lock around the desired state
  (simple and sufficient for now; per-app locks later if needed). A `start`
  with a spec identical to a running app's is idempotent; a different spec
  under the same name is refused (`Conflict`).
- A starting daemon or shim retries its lock for 500 ms, because other
  processes briefly take the lock to check whether its owner is alive.
- Single daemon per `$VEXOR_HOME`: `flock` on `daemon.lock`, held for the
  daemon's lifetime.
- No network listener by default. Optional metrics HTTP endpoint binds
  `127.0.0.1` only unless explicitly configured otherwise.

## 10. Logging

- Logs are written by the app's shim, not the daemon. A slow disk for one app
  can't stall other apps or the control plane, and logging continues while the
  daemon is down.
- The shim reads pipes in 8 KiB chunks into a bounded queue per log file
  (`buffer_bytes`, default 1 MiB). One writer thread per file does all disk
  I/O, so slow disks never block the restart loop. All instances (and
  stdout + stderr, when they share a path) go through the same writer, so
  chunks never interleave mid-write.
- Overflow policy, default `drop`: discarded bytes are counted, a
  `[vexor] dropped N bytes` marker is written when the writer catches up, and
  a `log_dropped` event is emitted (at most every 10 s per file). `block`
  (back-pressure: the app blocks on its own writes) is opt-in and loses
  nothing.
- A log file moved or deleted by an external tool (logrotate) is noticed
  within 1 s and reopened at its path.
- Built-in size-based rotation, on by default: `max_size_bytes` (10 MiB) and
  `retain` (5 rotated files), so at most ~60 MiB per log file. `retain: 0`
  truncates instead; `max_size_bytes: 0` disables rotation. (PM2 core has
  none; it needs the `pm2-logrotate` module.)
- Write failures (ENOSPC, EIO) → the app keeps running, a `log_error` event
  is emitted once per failure streak, the lost output is counted as
  dropped, and writing resumes (reopening the file) when the disk recovers.

## 11. Config loading

Accepted inputs: `ecosystem.config.{js,cjs,json}`, `*.json`, `*.yaml`, and
plain `vexor start <script>`. `.js` is evaluated with the **app's**
interpreter through `ecosystem-loader.js`. That runs arbitrary code, exactly as
PM2 does, and the docs say so. `.mjs` / ESM configs require Node ≥ 12 with
dynamic import support; on older Node they fail with a clear error.

Each field is classified as `supported`, `partial`, `ignored-safe`, or
`unsupported-fatal` (see compatibility.md). Unknown or unsupported fields always
produce a warning naming file + app + field. Fields whose absence would change
behaviour (e.g. `uid`, `cron_restart`) are fatal unless `--allow-unsupported`
is passed.

## 12. Non-goals

Multi-host orchestration, shared state between workers, exactly-once job
semantics, PM2 Plus / keymetrics integration, `pm2 deploy` (SSH deploy),
module system (`pm2 install`), and in-process APM (`pmx`, `axm_*`, `v8`,
`deep_monitoring`).

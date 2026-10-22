# Phased Plan

Each phase ends with green tests and updated docs. Phase N+1 doesn't start while phase N has failing mandatory tests.

## Phase 2: minimal reliable vertical slice (Linux + macOS)

Goal: `vexor start app.js --name api -i 1`, `stop`, `restart`, `delete`, `list --json`, `logs`, and `vexor daemon` with crash restart + backoff. No PM2 config yet.

| Step | Deliverable | Acceptance |
|---|---|---|
| 2.1 ✅ | Workspace skeleton (`crates/vexor` bin only; other crates are added in the step that first needs them), `rust-toolchain.toml` (1.99.1), CI: fmt, clippy -D warnings, test on linux x64/arm64 + macOS, cargo-deny; actions pinned by commit SHA | all checks green locally; CI not yet run (no GitHub remote) |
| 2.2 | `vexor-core`: state machine + restart policy (pure) | property tests: no transition sequence yields unbounded restarts within the budget window; every event in every state is handled |
| 2.3 | `vexor shim`: spawn (argv exec, setsid, env/cwd), pipes, NODE_CHANNEL_FD, identity capture, restart loop | integration: env/args/cwd round-trip; metachar args not shell-split |
| 2.4 | Stop: SIGTERM → kill_timeout → SIGKILL on pgid; shim subreaper orphan reap | test app ignoring SIGTERM is killed at timeout; grandchild cleaned up |
| 2.4b | Shim ↔ daemon protocol, buffered events, reconnect, daemon reconcile | **zero-downtime proof**: HTTP app under constant load (`wrk2`), kill -9 daemon × 100 in a loop → 0 failed requests, 0 app restarts, log lines contiguous; app crash while daemon is down → still restarted by shim |
| 2.5 | IPC server/client, flock single-daemon, peer-cred | second daemon refuses; wrong-UID connect refused |
| 2.6 | State store (atomic `state.json`) + resurrect on daemon start (policy R1/replace) | kill -9 daemon in a loop × 100 → no duplicate app instances, state consistent |
| 2.6b | `events.db` (SQLite): writer thread, lifecycle events, persisted restart budget, retention, `vexor events` | crash-looping app + daemon kill -9 mid-loop → budget still enforced after restart; corrupt db → daemon keeps supervising, flags degraded, file preserved |
| 2.7 | Log capture + size rotation + drop policy | 1 GB log flood stays under cap; daemon RSS bounded |
| 2.8 | Legacy runtime smoke: Node 8/12/16/20/24 docker | fork-mode suite green |

## Phase 3: npm distribution
ES5 wrapper, platform packages (linux-x64/arm64 musl, darwin-arm64), release workflow with checksums, attestations and SBOM, clean-machine install test, `install.sh` route.

## Phase 4: PM2 compatibility
Ecosystem loader (app's Node), field classifier + warnings, cluster-shim, health-gated `reload` (architecture.md §6a; acceptance: under constant `wrk2` load, cluster `-i 1` reload → 0 failed requests; replacement that never sends ready → old kept, reload reported failed, 0 failed requests; fork `start-first` on a fixed port → EADDRINUSE detected, old kept), `wait_ready`/`listen_timeout`, `save`/`resurrect`/`startup`, conformance fixtures diffed against PM2 7.x behaviour.

## Phase 5: migration
dry-run, backup, journaled apply, rollback (docs/migration.md).

## Phase 6: hardening
HTTP/TCP health checks, readiness-gated reload, max_memory_restart, watch, cron_restart, metrics endpoint, `doctor`, fault-injection suite, uid/gid review, Windows feasibility study.

## Phase 7: benchmarks (docs/benchmarks.md) · Phase 8: release readiness

## Decisions needed before Phase 2

1. ~~Daemon-crash recovery~~ **Decided:** per-app shim (R2), so daemon crash or upgrade causes zero app downtime and zero log loss (architecture.md §7, which also lists where zero downtime is impossible).
2. ~~State format~~ **Decided:** `state.json` for desired state + SQLite `events.db` for history, restart budgets and migration journal (§8).
3. Cluster mode via ES5 cluster-shim on app's Node (§6).
4. ~~License~~ **Decided:** `MIT OR Apache-2.0` (LICENSE-MIT, LICENSE-APACHE). PM2 is AGPL-3.0, so Vexor is a clean-room implementation built from docs and observed behaviour; no PM2 code is copied or translated.
5. ~~npm name~~ **Decided:** `vexord` (`npm i -g vexord`), command `vexor`, platform packages `vexord-<os>-<arch>`. Names to reserve before the public release.
6. ~~Rust toolchain~~ **Done:** rustup, stable 1.99.1, minimal profile + rustfmt + clippy, pinned in `rust-toolchain.toml`. Linux musl cross targets (cargo-zigbuild) are added in Phase 3.

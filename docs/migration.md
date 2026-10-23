# PM2 → Vexor Migration Design (Phase 1 draft)

## Sources inspected (read-only)

`$PM2_HOME` (default `~/.pm2`): `dump.pm2` (JSON array, one object per process, env flattened into top-level keys), `pids/`, `logs/`, ecosystem files referenced by the user, systemd/launchd units named `pm2-<user>`. Live state comes from `pm2 jlist` only if the user passes `--use-pm2-cli`. By default the dump file is used, so Vexor never talks to a running PM2 daemon unless asked.

## Commands

```
vexor migrate --from-pm2 --dry-run     # default when no mode is given; zero side effects
vexor migrate --from-pm2 --backup      # copies PM2 state to ~/.vexor/migrations/<ts>/ (0700), never overwrites
vexor migrate --from-pm2 --apply       # plan → confirm → cutover, journaled
vexor migrate --rollback <id>          # reverse a journaled migration
vexor migrate --status [<id>]
```

## Dry-run report (per app)

Implemented (plan 5.1): `vexor migrate --from-pm2 [--dry-run] [--pm2-home DIR] [--dump FILE] [--json]`. It reads `dump.pm2` only: no PM2 command is run, no Vexor app is added, and the Vexor daemon is not started. Exit code 0 when every app can be migrated, 4 when some are blocked, 1 when the dump can't be read (the message says to run `pm2 save`).

How the dump is read (checked against dumps written by PM2 7.1.0):

- One entry per instance: entries are grouped by namespace and name, and the instance count is the number of entries. Vexor has no namespaces, so one name in two namespaces is an error.
- Script, working directory and log files come from `pm_exec_path`, `pm_cwd` and `pm_out_log_path`/`pm_err_log_path`. Apps keep writing to PM2's log files. When PM2 wrote one file per instance (`api-out-0.log`, `api-out-1.log`), Vexor writes all instances to the set's base name (`api-out.log`) and says so.
- PM2's bookkeeping (`status`, `pm_uptime`, `unique_id`, `axm_*`, …) and settings PM2 writes into every entry with their default value (`pmx`, `automation`, `kill_retry_time: 100`, `namespace: default`, …) are dropped. Everything else goes through the same translator and field classification as `vexor check`.
- Environment: the dump's `env` holds the shell environment `pm2` was started from as well as the app's own variables, and PM2 can't tell them apart either. All of it is kept (minus PM2's own `PM2_*`, `unique_id`, `PWD`, `SHLVL` and the instance variable), so apps see what they see under PM2. The migrating shell's environment is not added. The report lists variable names only, never values.
- `username`: PM2 records its own user in every entry. An error only when it differs from the user running the migration.

Per app, the report shows the command line Vexor would run (absolute interpreter path), working directory, log files, environment variable names, every finding (error / warning / note), and the downtime class:

- `none`: not running under PM2 (stopped or errored), so nothing is interrupted.
- `brief`: running; stopped under PM2 then started under Vexor, so down for about the app's startup time.
- `blocked`: an unsupported setting that would change behaviour (`max_memory_restart`, `cron_restart`, `watch`, …), a missing working directory, script or interpreter, or another user.

Extra checks: the working directory and script exist; for Node.js apps, the Node.js Vexor would use has the same major version PM2 recorded (`node_version`); PM2 itself runs Node.js apps with its own Node.js, which the report points out.

Global findings: `dump.pm2` readable by other users (it holds every app's environment), a PM2 boot service (`/etc/systemd/system/pm2-<user>.service`, `~/Library/LaunchAgents/pm2.<user>.plist`), shell environment captured in the dump, and the dump's age (it should be saved right before migrating).

## Backup

Implemented (plan 5.2): `vexor migrate --from-pm2 --backup [--pm2-home DIR] [--dump FILE] [--json]`. PM2 is not touched and the daemon is not started.

- Copies `dump.pm2`, `dump.pm2.bak`, `module_conf.json` and `conf.js` from the PM2 home (when present) and PM2's boot service file, to `<vexor home>/migrations/<id>/` (`<id>` = UTC time, e.g. `20261023-053917`). With `--dump FILE` outside the PM2 home, only that file is copied.
- Also writes `plan.json` (the dry-run report, env values redacted) and `manifest.json` (source path, size, original mode and SHA-256 of each copy, SHA-256 of the plan). `--apply` will check the plan hash, so it applies exactly the plan the user reviewed.
- Directories are 0700 and files 0600 (the dump copy holds every app's environment). Sources are read without following a final symlink, must be regular files, and at most 64 MiB.
- Never overwrites: each backup gets a new id (`-2`, `-3`, … within the same second). It is written to a hidden temporary directory and renamed into place only when complete; on any error nothing is kept.
- Restoring PM2's process list by hand: `cp <backup>/pm2/dump.pm2 ~/.pm2/dump.pm2 && pm2 resurrect` (printed after each backup).

## Apply: cutover per app (sequential by default)

1. Pre-flight: backup exists, plan hash matches what the user confirmed, interpreter/cwd/script exist, Vexor daemon healthy.
2. Journal `begin(app)`.
3. `pm2 stop <app>`. Never `delete`, so PM2 still holds its definition for rollback.
4. `vexor start` with the translated config; wait for readiness (or `min_uptime`).
5. Healthy → journal `done(app)`. Unhealthy → `vexor delete`, `pm2 start <app>`, journal `rolled_back(app)`, stop the run.
6. After all apps: `pm2 save` is **not** run, and PM2 startup is **not** removed. The report tells the user how to disable it (`pm2 unstartup`) once satisfied. Two boot-time supervisors would double-start apps, so `vexor doctor` flags this state until it is resolved.

Cluster apps whose port is held by PM2 can't overlap with Vexor instances, so cutover is stop-then-start and the report says `brief` downtime. Zero downtime is never claimed for single-host migration. Ways to get it:

- **Multiple hosts behind a load balancer**: drain host → migrate → re-enable, one host at a time. `vexor migrate` prints this runbook when it detects a single-host cutover.
- **Apps that set `reusePort: true`** (Node ≥ 22.12/23.1, Linux): Vexor can start new instances while PM2's still listen, then stop PM2's. Detection is opt-in (`--overlap`), because Vexor can't verify from outside that an app uses `SO_REUSEPORT`. The dry run states that downtime is assumed unless the overlap succeeds.

Once migrated, later Vexor daemon crashes and upgrades don't cause downtime (architecture.md §7).

## Idempotency & partial failure

The journal (the `migration_steps` table in `events.db`, exported as `migrations/<id>/report.json` alongside the backup after every step) is the source of truth. Re-running `--apply` resumes from it. Apps already `done` are skipped, and apps `in_progress` are re-verified, never blindly restarted. `--rollback` walks the journal in reverse and asks for confirmation before each disruptive step.

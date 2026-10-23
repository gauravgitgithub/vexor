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

Per app, the report shows the command line Vexor would run (absolute interpreter path), working directory, log files, its environment, every finding (error / warning / note), and the downtime class (in the table, with the first blocking reason, e.g. `blocked: script missing`). Findings that apply to every app (the Node.js interpreter, captured shell environment) are said once at the top. Environments are summarised: when apps share one variable set (typical when one shell started `pm2` for all of them), the top says so once and each app lists only its differences (`+ added; − missing`); names that look like they come from the terminal, shell or npm (`SSH_AUTH_SOCK`, `TERM_SESSION_ID`, `npm_*`, …) are counted. `--env-names` lists every name of every app; values are never shown. Apps PM2 is restarting over and over (`waiting restart`) or has given up on (`errored`) get a warning, since they will behave the same under Vexor.

Downtime classes:

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

## Apply: cutover per app (sequential)

Implemented (plan 5.3): `vexor migrate --from-pm2 --apply [--backup-id ID] [--only a,b] [--yes] [--pm2 PATH] [--timeout SECS]`.

1. Pre-flight: the backup (newest, or `--backup-id`) exists; PM2's current `dump.pm2` has the same SHA-256 as the backup's copy (otherwise: back up again and review the new plan); the backup's `plan.json` matches its manifest hash. Blocked apps are listed and stay on PM2. The plan is shown and confirmed (`--yes` when not on a terminal). One `--apply` per backup at a time (lock file).
2. Journal `begin(app)`, after checking Vexor has no app by that name (if it has, PM2 is not touched and the run stops).
3. Running under PM2: `pm2 stop <app>` (with `PM2_HOME` set). Never `delete`, so PM2 still holds its definition for rollback. A failing `pm2 stop` stops the run; nothing changed for that app.
4. Start under Vexor with the translated spec (apps PM2 had stopped are added stopped). Healthy = every instance `online`, no restart, and the same live processes for `min_uptime` (at least 1 s, at most 15 s), within `--timeout` (default 30 s) plus `listen_timeout`.
5. Healthy → journal `done(app)`. Unhealthy → `vexor delete`, `pm2 start <app>`, journal `rolled_back(app)` with the reason, stop the run (exit 1). Exception: an app PM2 was already restarting over and over (`waiting restart`) or had given up on (`errored`) says nothing about the move, so after putting it back the run continues, and it ends with exit 4 listing the apps left on PM2. If putting it back fails too: `failed(app)`, and the message says to check `pm2 ls` and `vexor ls`.
6. After all apps: `pm2 save` is **not** run, and PM2 startup is **not** removed. PM2's saved list still has the apps as running, so the report tells the user to run `pm2 unstartup` (then `vexor startup`): two boot-time supervisors would double-start apps.

The journal is `journal.jsonl` in the backup directory (0600, one JSON line per step, flushed to disk before the next step; a torn last line is ignored). Re-running `--apply` resumes: apps `done` are skipped; an app interrupted mid-move (`begin` / `pm2_stopped` / `vexor_started`) is continued (`pm2 stop` again, start the Vexor app if present, health check), never started twice; an app `rolled_back` is tried again.

Measured (PM2 7.1.0, Docker, `node:22-alpine`): cluster app with 2 instances on port 3000, fork app and a stopped app moved in 3 s; an HTTP probe every 50 ms saw 454 successes and 2 failures, all during the cluster app's stop-to-start gap (about 240 ms).

Cluster apps whose port is held by PM2 can't overlap with Vexor instances, so cutover is stop-then-start and the report says `brief` downtime. Zero downtime is never claimed for single-host migration. Ways to get it:

- **Multiple hosts behind a load balancer**: drain host → migrate → re-enable, one host at a time. `vexor migrate` prints this runbook when it detects a single-host cutover.
- **Apps that set `reusePort: true`** (Node ≥ 22.12/23.1, Linux): Vexor can start new instances while PM2's still listen, then stop PM2's. Detection is opt-in (`--overlap`), because Vexor can't verify from outside that an app uses `SO_REUSEPORT`. The dry run states that downtime is assumed unless the overlap succeeds.

Once migrated, later Vexor daemon crashes and upgrades don't cause downtime (architecture.md §7).

Health is judged within the `min_uptime` window (at least 1 s). An app that crashes later, or whose crash is reported after the window on a heavily loaded host, counts as migrated; Vexor's restart policy then handles it like any other app, and `--rollback` moves it back.

## Rollback and status

Implemented (plan 5.4).

`vexor migrate --rollback [ID] [--only a,b] [--yes] [--pm2 PATH] [--timeout SECS]` undoes a migration (default: the newest backup). From the journal it takes every app with a `begin` that wasn't rolled back since, newest first, and for each (after confirmation, or `--yes`): deletes the Vexor app by that name (it can only be the migrated one: `begin` is written after checking Vexor had no such app), then, if `pm2 stop` had run, `pm2 start <app>` and waits until every PM2 instance is `online` (`pm2 jlist`). Each app is journaled `rolled_back`; if PM2 can't start it, `failed`, and the rollback stops. Apps refused before `begin` (name taken in Vexor) and apps already rolled back are skipped, so rolling back twice is harmless, and a rolled-back app can be migrated again with `--apply`. Afterwards: run `pm2 startup` again if it was removed.

`vexor migrate --status [ID] [--json]` lists backups and, per app, its last journal step: migrated, back on PM2, failed, or interrupted (continue with `--apply` or undo with `--rollback`).

Measured (PM2 7.1.0, Docker): rolling back the three apps above took under a second; the cluster app's HTTP probe saw 3 of 273 requests fail during the hand-back.

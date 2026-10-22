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

Name, script, interpreter (resolved absolute path + version via `<node> -v`), cwd, exec_mode/instances, the field classification from compatibility.md, env key **names** only (values never printed), log paths, expected downtime class:

- `rolling`: cluster with ≥ 2 instances and readiness configured
- `brief`: single instance; a stop/start gap equal to the app's startup time
- `blocked`: unsupported-fatal fields, missing interpreter, cwd missing, port conflict

Plus global findings: startup unit present, dump file permissions, env captured from an interactive shell.

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

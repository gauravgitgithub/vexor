# Competitive Assessment (checked 2026-10-22)

Sources: `npm view`, GitHub API, and project READMEs/docs. Competitor performance claims are hypotheses until we reproduce them.

| | PM2 | OxMgr | PMC | Super |
|---|---|---|---|---|
| Language / license | JS / **AGPL-3.0** | Rust / MIT | Rust / MIT | Rust / MIT (some paid "Pro" plugins) |
| Latest release | v7.1.0 (2026-10-21) | v0.5.0 (2026-07-09), active | v1.8.0 (2024-06-28), stale | v1.7.0 (2026-09-27) |
| PM2 compat | — | reads ecosystem.config.{js,cjs,mjs,json}, `import`/`convert` to TOML | similar verbs, no ecosystem | none |
| OS | Linux/macOS/Windows | Linux/macOS/Windows | Linux/macOS | cross-platform |
| Ops | cluster, reload, wait_ready, startup; rotation via `pm2-logrotate` module | restart policies, health checks, readiness reload, built-in rotation, doctor, cluster, TUI, git/webhook deploy | basic lifecycle, HTTP API, remote | TCP/HTTP/exec probes, `reload --wait-healthy`, atomic deploy + rollback, cron, Prometheus, SQLite ledger |
| Perf claims | none | CI benchmark: idle daemon 6.6 MB vs PM2 57 MB; 100 procs 10 MB vs 167 MB; crash→ready 36 ms vs 181 ms (GitHub runner, noisy) | none | none |
| npm install | JS | `postinstall` downloads binary | — | — |
| Node engines | v2 ≥0.12, v3 ≥6, v4 ≥8.10, v5 ≥12, v6 ≥16, **v7 ≥18** | ≥18 | — | — |

Native supervisors: systemd covers restart limits, `Type=notify` readiness, socket activation, cgroups and journald caps, but has no Node cluster fan-out or ecosystem story. supervisord, s6 and runit are fine generic supervisors with no Node tooling.

PM2 pain points with open issues: God daemon memory leak (#6113), cluster reload downtime (#3143), `wait_ready` marking failed launches online (#5013, #5716, #5632), `startup` path bugs (#5911, #6010), logrotate ignoring `$PM2_HOME` (#2859).

## Conclusion

A low-RSS Rust daemon, restart policies, health checks, readiness reload, built-in rotation, `doctor`, TUI and ecosystem import are **commoditized**: OxMgr already ships them under MIT, with PM2 benchmarks. Vexor can't win on "Rust is faster."

Gaps Vexor can defend:

1. **Legacy Node fleets.** PM2 ≥ 7 and OxMgr both require Node ≥ 18. Teams on Node 8–16 are stuck on old PM2 majors. Vexor's supervisor has no Node dependency, its wrapper targets Node ≥ 10, and a direct binary install covers anything older.
2. **Live PM2 migration with rollback.** No `dump.pm2` migration was found in OxMgr (based on code search, so likely but not certain). Dry-run, backup, journaled cutover and rollback are unoccupied.
3. **Faithful PM2 semantics instead of a new config dialect.** OxMgr cluster mode rejects `node_args`, and its TOML is the primary format. Vexor keeps ecosystem files as the first-class format and runs a conformance suite against PM2.
4. **Install without scripts.** Platform `optionalDependencies` (esbuild/biome pattern) work under `--ignore-scripts` and locked-down CI. OxMgr uses `postinstall`.
5. **Self-hosted alerting and an incident timeline.** PM2's free `monit` is a live terminal view with no history and no alerts; alerting is in the paid PM2 Plus. Vexor plans local alerts (warning/danger/recovered over email and webhook) and a per-process timeline, with per-route attribution from the app's existing JSON logs (plan Phase 6b). Whether OxMgr or Super already cover this hasn't been checked yet; to verify before the claim goes public.
6. **Secure defaults vs PM2**: 0600 state, no shell-env capture, redaction (see security.md). PID-identity hardening only becomes a claim once tested.

Not building: SaaS/APM, AI features, git/webhook deploy, remote multi-host control plane, a cgroup/journald replacement (generate systemd units instead), `pm2 serve`, a module marketplace, benchmark-led marketing.

## Naming

**Decided: npm package `vexord`** (free on npm and crates.io as of 2026-10-22). `vexor` itself is taken by a deprecated payments SDK (v0.2.8, maintainer vectrals.com). The product name stays Vexor, and the installed command is `vexor`. Platform binaries ship as `vexord-<os>-<arch>` (e.g. `vexord-linux-x64`), all free as of 2026-10-22. Reserve these names before the first public release.

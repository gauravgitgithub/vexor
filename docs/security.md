# Vexor Threat Model (Phase 1 draft)

Assets: control of managed processes, app secrets (env), log contents, the binary itself, the host user account.

## Observed PM2 baseline (one real developer machine, PM2 7.0.4, macOS)

These observations come from inspecting `~/.pm2` metadata. No secret values were read or recorded.

| Observation | Risk |
|---|---|
| `dump.pm2` is mode 0644 and stores full app env at top level (≈240 keys including cloud credentials, DB passwords, API keys) | Any local user can read secrets |
| `rpc.sock` / `pub.sock` mode 0775 | Group members can control the daemon |
| `pm2.log` 30 MB, `logs/` 245 MB, single files up to 45 MB, no rotation | Disk exhaustion |
| `dump.pm2` captured the interactive shell env (TERM_*, SSH_AUTH_SOCK, STARSHIP_*) | Env leakage and non-reproducible restarts |

Vexor defaults address each row. These are design targets until tests verify them.

## Threats and mitigations

| # | Threat | Mitigation | Test |
|---|---|---|---|
| T1 | Another local user controls the daemon | `$VEXOR_HOME` 0700, socket 0600, peer-UID check on every connection | connect as second user → refused |
| T2 | Secrets leak via state/dump files | state files 0600; `save` stores only explicit app env, not the caller's whole shell env (opt-in `--capture-env` for PM2 parity) | file modes asserted; diff of saved keys |
| T3 | Secrets leak via `describe`/`doctor`/logs/metrics/migration reports | values redacted by default; `--show-env` needs explicit flag; metrics carry no env/args | golden-output tests with canary secrets |
| T4 | Shell injection | no shell: `execve` with argv array. `script` with spaces is not split. Shell only if `interpreter: "sh"` is explicitly set | fuzz args with metacharacters |
| T5 | Symlink / path traversal in log & state paths | open with `O_NOFOLLOW` + `O_CLOEXEC`; create in 0700 dirs; reject relative paths escaping cwd for config-supplied paths unless absolute | symlink-swap tests |
| T6 | PID reuse → signalling a wrong process | identity check (pid + start time), pidfd on Linux | PID-reuse simulation |
| T7 | Malicious ecosystem.js | it is code, same as PM2: documented. Loader runs as the invoking user, never as root via the daemon | doc + test that daemon never evaluates JS itself |
| T8 | Privilege escalation via root daemon | no root needed for normal operation; `uid/gid` drop is out of scope until reviewed | — |
| T9 | Tampered binary / supply chain | no postinstall, binaries ship inside npm platform packages (npm provenance); release tarballs with SHA-256 + GitHub artifact attestations; `cargo-deny`/`cargo-audit` + `npm audit` in CI; SBOM (CycloneDX) per release | release workflow checks |
| T10 | Resource exhaustion (restart storm, log flood, IPC flood) | crash-loop budget, bounded log buffers + disk caps, IPC frame limit 4 MiB, per-connection rate limit | fault-injection suite |
| T11 | Optional metrics endpoint exposed | binds 127.0.0.1 by default; non-loopback requires explicit config + warning | bind test |

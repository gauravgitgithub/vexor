# Faults and what happens

Each row is a fault injected by a test, and the outcome the test checks.
Run them all with `cargo test -p vexor --test daemon fault_` plus the tests
named in the last column.

| Fault | What happens | Test |
|---|---|---|
| Daemon killed (`kill -9`), 100 times in a row, under HTTP load | No failed request, same app process, one shim, every request logged once and in order | `zero_downtime_across_100_daemon_kills` |
| App crashes while the daemon is down | Its shim restarts it; the daemon learns about it when it returns | `shim_restarts_crashed_app_while_daemon_is_down` |
| Daemon hung (`SIGSTOP`) | Apps keep running and keep being restarted. CLI questions (`ls`, `describe`, `events`, `metrics`, `routes`, `alerts`) fail after 10 s with what to do, instead of waiting 5 minutes. Everything works again when the daemon resumes | `fault_hung_daemon` |
| One app's shim hung (`SIGSTOP`) | The daemon and the other apps carry on; commands for the other apps work | `fault_hung_shim` |
| A shim killed (`kill -9`), fork mode | Its processes keep running. The daemon starts a new shim, which adopts them (same PIDs, identity-checked, never duplicated) and supervises them: restarted when they exit. Their output went to the dead shim and isn't captured until they restart; `describe` and `doctor` say so | `a_dead_shims_instances_are_adopted_not_duplicated` |
| A shim killed, cluster mode | The orphaned cluster primary can't be controlled (its channel died with the shim): the new shim stops it and its workers, then starts the app again. Nothing is left listening twice | `a_dead_shims_cluster_is_stopped_and_started_again` |
| Shim replaced by a newer version (in-place upgrade), under HTTP load | No failed request; same app process, still connected to its IPC channel; output keeps flowing with nothing lost or repeated; the event stream continues without a gap; a later crash is still seen with its exit signal and restarted | `tests/upgrade.rs` |
| In-place upgrade to a program that can't run | The shim carries on in its current program from the same handover; apps untouched; an `upgrade_skipped` event says why | `a_failed_exec_resumes_in_the_same_program` |
| Event database corrupted | The daemon keeps the bad file (`events.db.corrupt-<ts>`), starts a fresh one, warns on every command, and supervises as before | `fault_corrupt_event_database`, `corrupt_file_is_kept_and_replaced` |
| Event database location unusable | Supervision continues without history; reported as degraded | `unusable_location_degrades_without_failing` |
| Log file unwritable (here: replaced by a directory) | A `log_error` event, once; the app keeps running; logging resumes when the path is usable again | `fault_unwritable_log_file` |
| Log file moved or deleted by another tool (logrotate) | Reopened at its path within a second | `reopens_after_external_move` |
| App writes faster than the disk takes it | `overflow: drop` (default): excess dropped, counted, marked in the log and reported; the app never waits. `overflow: block`: the app waits, nothing is lost | `drop_mode_counts_and_marks_lost_output`, `block_mode_rotation_keeps_every_line_in_order`, `one_gigabyte_flood_stays_under_the_cap_with_bounded_memory` |
| Reload whose new process never becomes ready | The old process keeps serving; no failed request | `failed_reload_keeps_the_old_process_and_loses_no_requests` |
| Cluster primary killed | Every worker is restarted under a new primary | `cluster_primary_death_restarts_every_worker` |
| App ignores the stop signal | SIGKILL after `kill_timeout`; its process group is cleared | `app_ignoring_sigterm_is_killed_after_kill_timeout`, `stop_kills_group_members_that_ignore_sigterm` |
| `state.json` corrupted, or written by a newer Vexor | Refused and kept, never overwritten | `corrupt_or_newer_files_are_refused_and_kept` |
| Busy machine: a new process waits seconds before it runs (seen on macOS at `_dyld_start`) | Not a Vexor fault, but tests must not assume a crash shows within a fixed time; the migrate tests allow for it | `migrate_apply_*` in `tests/cli.rs` |

Not covered yet: a full disk for the event database, a shim killed in the
middle of an in-place upgrade (the next shim adopts its processes, as for
any dead shim, but this isn't tested), and network filesystems (refused by
`doctor`).

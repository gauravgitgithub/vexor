# Alerts

Vexor watches every running app and opens an **incident** when something
needs attention: a crash loop, frequent restarts, a failing health check,
log files filling the disk, or (once you set thresholds) high CPU, memory
or open files. Everything runs locally in the daemon; nothing is sent
anywhere unless you configure a channel (docs/plan.md 6b.4).

```sh
vexor alerts          # open incidents, the last 7 days of alerts, the rules
vexor alerts --json
vexor events          # alerts also appear in the event history
```

## How an incident behaves

- A level (`warning` or `danger`) must hold for the rule's `for` before the
  incident opens or escalates. A short spike opens nothing.
- It notifies when it opens, when it escalates to danger and when it
  recovers: never twice for the same level.
- It recovers once the value has stayed below the warning level for the
  same `for`.
- An incident that opens 3 times within an hour is **flapping**: one
  `flapping` notice, then silence (still recorded, shown as "quiet") until
  it has stayed clear for the cooldown, then one `recovered`.
- Stopping, deleting or scaling down an app ends its incidents without a
  notice.

## Built-in rules

These apply with no configuration:

| Rule | Measures | Default |
|---|---|---|
| `crash_loop` | an instance gave up restarting (`errored`) | danger at once |
| `restarts` | restarts of an instance in the last hour | warning at 3, danger at 10 |
| `unhealthy` | health check failing, or instance unhealthy | danger after 30 s |
| `log_disk` | the app's log files on disk, rotated ones included | warning at 1 GiB, danger at 5 GiB |
| `cpu` | CPU over the sampling interval, % of one core | off |
| `memory` | resident memory | off |
| `open_files` | open file descriptors | off |

CPU, memory and open files depend on the app, so they are off until you
give them a threshold.

## `alerts.json`

Put it in the Vexor home (`~/.vexor/alerts.json` by default). The daemon
re-reads it when it changes. If it is invalid, the previous rules stay in
force and every `vexor` command shows a warning until it is fixed.

```json
{
  "cooldown": "10m",
  "rules": {
    "memory":   { "warning": "512M", "danger": "900M", "for": "2m" },
    "cpu":      { "warning": "80%", "danger": "95%", "for": "5m", "apps": ["api"] },
    "restarts": { "warning": 5 },
    "log_disk": { "enabled": false }
  }
}
```

Each rule overrides the built-in one for its metric, field by field:

| Field | Meaning |
|---|---|
| `warning`, `danger` | thresholds: a number, a size (`512M`, `1G`, powers of 1024) or a percentage (`80%`) |
| `for` | how long a level must hold: `30s`, `5m`, `1h`, `500ms`, or milliseconds |
| `apps` | only these apps (default: all) |
| `enabled` | `false` turns a rule off; giving a threshold turns an off rule on |

`cooldown` (default `10m`) is how long a flapping incident must stay clear
before it notifies `recovered`.

Unknown fields and metrics are errors, so a typo never silently disables a
rule.

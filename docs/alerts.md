# Alerts

Vexor watches every running app and opens an **incident** when something
needs attention: a crash loop, frequent restarts, a failing health check,
log files filling the disk, or (once you set thresholds) high CPU, memory
or open files. Everything runs locally in the daemon; nothing is sent
anywhere unless you configure a channel (email or webhook, below).

```sh
vexor alerts          # open incidents, the last 7 days of alerts, the rules
vexor alerts --json
vexor alerts test     # send a test alert to every channel and report each result
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

## Channels: email and webhooks

Incidents are delivered when they open, escalate, recover or start
flapping. Delivery runs on its own thread: a slow or unreachable server
never delays supervision. A failed delivery is retried after 5 s, 30 s,
2 min, 10 min and 30 min, then given up and recorded as an
`alert_delivery_failed` event. While a channel fails, every `vexor`
command shows a warning.

```json
{
  "channels": {
    "email": {
      "host": "mail.example.com",
      "port": 587,
      "tls": "starttls",
      "username": "vexor@example.com",
      "password_file": "/etc/vexor/smtp-password",
      "from": "vexor@example.com",
      "to": ["ops@example.com"],
      "levels": ["danger", "recovered"]
    },
    "webhooks": [
      { "url": "https://hooks.slack.com/services/T000/B000/XXXX", "format": "slack" },
      { "url": "https://ops.example.com/vexor", "secret_file": "/etc/vexor/webhook-key" }
    ]
  }
}
```

Email (any SMTP server: Stalwart, Postfix, a provider's relay):

| Field | Meaning |
|---|---|
| `host`, `port` | the SMTP server; the port defaults to 587 (`starttls`), 465 (`implicit`) or 25 (`none`) |
| `tls` | `starttls` (default; the server must offer it), `implicit` (TLS from the first byte), or `none` (only for a server on this host: the password would cross the network in clear) |
| `username`, `password` / `password_file` | AUTH PLAIN, or LOGIN if that is all the server offers; a file holds the password on its first line |
| `from`, `to` | sender and recipients |
| `ca_file` | extra CA certificate (PEM) to trust, for a server with a private CA; the system's trusted CAs are always used |
| `accept_invalid_certs` | skip certificate checks; testing only, and every command warns while it is on |
| `levels` | only these of `warning`, `danger`, `recovered`, `flapping` (default: all) |

Webhooks receive a POST:

- `format: "json"` (default): `{"type": "alert", "alert": {...}, "text": "..."}`
  with the app, instance, rule, level, value and host.
- `format: "slack"`: `{"text": "..."}`, for Slack incoming webhooks and the
  many services that accept the same shape.
- With `secret` / `secret_file`, requests carry `X-Vexor-Timestamp` and
  `X-Vexor-Signature: sha256=<hex>`, an HMAC-SHA256 of
  `"<timestamp>.<body>"` with the secret. Check it, and reject old
  timestamps, to know a request came from your Vexor and isn't a replay.

Secrets are never logged, never included in errors and shown as
`<redacted>` by `vexor alerts --json`. If `alerts.json` configures
channels and is readable by other users, every command warns: run
`chmod 600` on it, or use `password_file` / `secret_file`.

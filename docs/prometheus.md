# Prometheus

Vexor can serve its view of every app at `GET /metrics` in the Prometheus
text format, for an existing Prometheus and Grafana setup. It is off by
default.

`$VEXOR_HOME/settings.json` (`~/.vexor/settings.json` by default):

```json
{ "prometheus": { "listen": "127.0.0.1:9615" } }
```

The daemon re-reads the file within 5 seconds of a change; no restart
needed. The endpoint has no authentication, so keep it on loopback and
let a local Prometheus or agent scrape it. Any other address works, and
every `vexor` command warns while it is in use.

```yaml
# prometheus.yml
scrape_configs:
  - job_name: vexor
    static_configs:
      - targets: ["127.0.0.1:9615"]
```

## Metrics

Per instance, labelled `app` and `instance`:

| Metric | Type | Meaning |
|---|---|---|
| `vexor_instance_up` | gauge | 1 when online |
| `vexor_instance_state` | gauge | 1, with the current state in a `state` label (`online`, `backoff`, `errored`, `stopped`, ...) |
| `vexor_instance_restarts_total` | counter | restarts since the app's supervisor started |
| `vexor_instance_uptime_seconds` | gauge | time since the instance came online |
| `vexor_instance_cpu_percent` | gauge | CPU over the last sampling interval, % of one core |
| `vexor_instance_memory_bytes` | gauge | resident memory |
| `vexor_instance_open_files` | gauge | open file descriptors |
| `vexor_instance_health_ok` | gauge | latest health check: 1 passed, 0 failed (apps with `health_check`) |

And:

| Metric | Type | Meaning |
|---|---|---|
| `vexor_alerts_open` | gauge | open alert incidents, by `rule` and `level` (docs/alerts.md) |
| `vexor_build_info` | gauge | 1, with the Vexor `version` label |

CPU, memory and open files come from the samples the shims take every
`metrics_interval_ms` (5 s by default); restarts, states and uptime come
from the lifecycle events, so they are exact.

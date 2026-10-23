Measured 2026-10-23 on Apple M5 (Docker Desktop VM) (10 cores, 8 GB), linux/arm64 7.0.12-linuxkit, in a container, Node v22.23.4. Each tool runs alone, installed from npm, on the same apps. **Bold** = best; n/a = not supported or failed (see notes).

| Measure | Vexor 0.3.1 | PM2 7.1.0 | OxMgr 0.5.0 | forever 4.0.3 |
|---|---|---|---|---|
| List apps (CLI), median | **23 ms** | 76 ms | 24 ms | 109 ms |
| Start 10 apps until online, median | 352 ms | 435 ms | **317 ms** | 1268 ms |
| Supervisor memory (PSS), 1 app | 5.7 MiB | 33 MiB | **3.5 MiB** | 37 MiB |
| Supervisor memory (PSS), 10 apps | 7.9 MiB | 21 MiB | **5.7 MiB** | 191 MiB |
| Supervisor memory (PSS), 50 apps | 17 MiB | 21 MiB | **4.6 MiB** | 825 MiB |
| Supervisor memory (RSS), 1 app | 8.1 MiB | 66 MiB | **3.5 MiB** | 67 MiB |
| Supervisor memory (RSS), 10 apps | 33 MiB | 68 MiB | **5.7 MiB** | 675 MiB |
| Supervisor memory (RSS), 50 apps | 140 MiB | 72 MiB | **4.6 MiB** | 3316 MiB |
| Supervisor CPU while idle, 1 app | **0.00%** | 0.37% | 0.07% | 0.37% |
| Supervisor CPU while idle, 10 apps | **0.00%** | 0.70% | 0.07% | 2.26% |
| Supervisor CPU while idle, 50 apps | 0.33% | 1.50% | **0.17%** | 7.48% |
| Crash to back online, median | 38 ms | 159 ms | 20 ms | **13 ms** |
| Failed requests in 5 cluster reloads | **0** | **0** | **0** | n/a |
| Supervisor daemon killed: failed requests | **0** | 739 | **0** | n/a |
| Supervisor daemon killed: app still managed | **yes** | no | **yes** | n/a |
| Installed size (npm) | 5.8 MiB | 17 MiB | **3.9 MiB** | 20 MiB |

Requests sent: reloads Vexor 1615, PM2 1726, OxMgr 1645, forever 0; daemon kill Vexor 1239, PM2 1219, OxMgr 1220, forever 0.

Notes:
- OxMgr, crash_restart_ms: Measured separately: OxMgr marks an app errored after its 3rd crash by default (crash_restart_limit 3), even with uptime between crashes, so only 3 restarts can be timed.

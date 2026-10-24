Measured 2026-10-24 on Apple M5 (colima VM) (4 cores, 4 GB), linux/arm64 6.8.0-117-generic, in a container, Node v22.23.4. Each tool runs alone, installed from npm, on the same apps. **Bold** = best; n/a = not supported or failed (see notes).

| Measure | Vexor 0.10.1 | PM2 7.1.0 | OxMgr 0.5.0 | forever 4.0.3 |
|---|---|---|---|---|
| List apps (CLI), median | 14 ms | 58 ms | **13 ms** | 86 ms |
| Start 10 apps until online, median | 339 ms | 362 ms | **268 ms** | 1005 ms |
| Supervisor memory (PSS), 1 app | 5.5 MiB | 54 MiB | **3.4 MiB** | 30 MiB |
| Supervisor memory (PSS), 10 apps | 7.9 MiB | 37 MiB | **3.6 MiB** | 186 MiB |
| Supervisor memory (PSS), 50 apps | 19 MiB | 33 MiB | **4.5 MiB** | 820 MiB |
| Supervisor memory (RSS), 1 app | 8.2 MiB | 130 MiB | **3.4 MiB** | 67 MiB |
| Supervisor memory (RSS), 10 apps | 35 MiB | 132 MiB | **3.6 MiB** | 672 MiB |
| Supervisor memory (RSS), 50 apps | 153 MiB | 134 MiB | **4.5 MiB** | 3359 MiB |
| Supervisor CPU while idle, 1 app | **0.00%** | 0.37% | 0.03% | 0.20% |
| Supervisor CPU while idle, 10 apps | **0.07%** | 0.70% | 0.07% | 2.50% |
| Supervisor CPU while idle, 50 apps | **0.20%** | 2.10% | 0.30% | 11.77% |
| Crash to back online, median | 47 ms | 131 ms | 41 ms | **18 ms** |
| Failed requests in 5 cluster reloads | **0** | **0** | 2 | n/a |
| Supervisor daemon killed: failed requests | **0** | 730 | **0** | n/a |
| Supervisor daemon killed: app still managed | **yes** | no | **yes** | n/a |
| Installed size (npm) | 4.9 MiB | 17 MiB | **3.9 MiB** | 20 MiB |

Requests sent: reloads Vexor 1592, PM2 1565, OxMgr 1598, forever 0; daemon kill Vexor 1154, PM2 1210, OxMgr 1202, forever 0.

Notes:
- OxMgr, crash_restart_ms: only 3 restarts timed: it marks an app errored after its 3rd crash by default, even with uptime between crashes

# Granular-budget resource checks

The exact `635ec740` candidate passes five interleaved 2 GiB connection-burst pairs against retained Rust `0be60915`, three additional candidate startup pilots, and twelve sort/restart trials against native PostgreSQL 18.6. This is local resource evidence, not production qualification. No GitHub Actions were used.

The ten paired trials accept 16,000 connections across two waves per trial and reject all twenty expected overflow attempts with SQLSTATE 53300. Startup CPU per accepted connection falls 10.23% by the medians, or 8.63% including shutdown. One pair uses 1.03% more startup CPU (4.72% including shutdown); all pairs and both median metrics stay inside the predeclared 5% guards. Peak charged memory falls 0.90% by the medians. Wave durations include client orchestration and diagnostics and do not measure standalone authentication latency.

All three additional 2 GiB pilots accept 1,600 connections each, with 487.11 MiB minimum headroom. All thirteen connection trials have clean exits, no missing process samples, no timed server/driver/parent throttling, and no swap, OOM or memory-limit event. Hard kernel CPU sets constrain four physical server cores and two separate driver cores without CFS quota.

The sort matrix uses 2, 4 and 8 GiB server limits, fresh sessions and loader-session reuse, and both engines. Each trial executes twelve alternating 64 KiB serial-spill and 16 MiB parallel sorts of one million wide rows, verifies the full ordered digest, checks temporary-file cleanup and connection/transaction recovery, then restarts and checks the full ordered data again. All 144 sorts and twelve restart checks pass. These single trials per combination are functional evidence, not a performance comparison.

| Server limit | PostgreSQL charged peak range | Rust charged peak range |
|---|---:|---:|
| 2 GiB | 690.82–691.20 MiB | 1197.71–1233.08 MiB |
| 4 GiB | 690.60–690.92 MiB | 1243.25–1250.55 MiB |
| 8 GiB | 690.64–690.86 MiB | 1242.36–1243.03 MiB |

Both engines finish without swap, OOM, memory-limit events or leaked server processes. The larger Rust memory footprint remains visible. The separate [1 GiB pressure and workload archive](../granular-budget-reclaim-2026-10-31/README.md) preserves retained-build OOM/watchdog failures and the repeated CPU/latency comparison.

Run `python3 replay.py` with assertions enabled to verify all archived bytes and recompute the paired summaries. It checks exact binaries and source, CPU binding, limits, all startup counts, overflow behavior, sort settings, full digests, spill witnesses and restart results. Changes to a binary identity, driver throttling, restart digest and sort setting must be rejected. Replay passes on Windows and Linux and does not execute archived code or rerun workloads.

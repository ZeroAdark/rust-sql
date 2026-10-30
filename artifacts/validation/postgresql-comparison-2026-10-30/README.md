# Native PostgreSQL comparison — 2026-10-30

For this synthetic mixed workload, Rust SQL uses about 91% less server CPU per completed unit of work, with much faster analytical queries. It uses more memory and has slower point-read and update latency at the shared moderate request rate. This is a workload-specific measurement, not a production-readiness sign-off.

## Configuration and scope

- Linux x86-64 in Ubuntu 24.04 / WSL2, kernel 6.6.87.2, AMD Ryzen 7 9800X3D. Four distinct server cores (logical CPUs 0,2,4,6); two different client cores (8,10). Compilation was stopped during every measured trial and resumed afterward. This has not been repeated on bare-metal production Linux.
- Both engines: 2 GiB server cgroup limit, no swap, 128 MiB shared buffers, 16 MiB work_mem, max_parallel_workers_per_gather=2, fsync=on, synchronous_commit=on, io_method=sync. Autovacuum is off for this bounded benchmark. Ordinary application roles and persistent prepared statements are used. Each trial uses a fresh private cluster.
- Native PostgreSQL 18.6 uses heap tables. Rust SQL uses heap for 120,000 transactional accounts and the 128-row dimension, and its columnar access method for one million analytical rows. The analytical columns are integer id, id modulo 128, and a periodic integer value. This narrow, ordered, highly compressible data favors columnar execution; wider, random, skewed, write-heavy and contended datasets remain to be measured.
- Every fixed bundle has 12 indexed point reads, four durable updates, one full-table aggregate, one 4,096-row range query and one aggregate join. Although most statement calls are transactional, full-table analytics dominate CPU cost. These are not TPC-C or TPC-H results, nor an isolated comparison of the Rust and C languages.
- Five alternating engine pairs for each phase. Reported figures are medians across five trials; percentile rows are medians of each trial's percentile, not pooled percentiles. Each unpaced trial measures 300 bundles; each moderate-load trial measures 180 bundles. Every trial warms up with 60 unmeasured bundles.
- Moderate load is 9.4 bundles/second, or 178.6 offered statements/second, rounded from half the slower engine's median unpaced throughput. Both receive the same rate and fixed work. Small differences in observed throughput come from the final bundle's completion time.
- Client statement latency spans execute through fetching the full result. Scheduled bundle-start delay is retained separately; the maximum is below one per-client interarrival interval in every run. No scheduled bundle is dropped. The short sample sizes do not establish long-duration tail-latency SLOs.

## Moderate-load measurements

| Metric | PostgreSQL 18.6 | Rust SQL | Rust SQL change |
|---|---:|---:|---:|
| Server CPU per 19-statement bundle | 196.89 ms | 17.93 ms | -90.9% |
| Server CPU / four-core budget | 46.33% | 4.23% | -90.9% |
| Sampled peak cgroup memory (including charged cache) | 223.75 MiB | 479.77 MiB | +114.4% |
| Sampled peak process PSS | 92.60 MiB | 507.13 MiB | +447.6% |
| Indexed point read p50 | 0.171 ms | 0.232 ms | +36.2% |
| Indexed point read p95 | 0.314 ms | 0.393 ms | +25.0% |
| Indexed point read p99 | 0.405 ms | 0.527 ms | +30.3% |
| Durable update p50 | 0.702 ms | 0.783 ms | +11.6% |
| Durable update p95 | 1.117 ms | 1.159 ms | +3.8% |
| Durable update p99 | 1.225 ms | 1.294 ms | +5.6% |
| Aggregate p50 | 25.036 ms | 3.617 ms | -85.6% |
| Aggregate p95 | 28.344 ms | 3.980 ms | -86.0% |
| Aggregate p99 | 31.710 ms | 4.171 ms | -86.8% |
| Range scan p50 | 9.814 ms | 0.732 ms | -92.5% |
| Range scan p95 | 10.586 ms | 0.895 ms | -91.5% |
| Range scan p99 | 12.325 ms | 1.143 ms | -90.7% |
| Join p50 | 36.308 ms | 5.032 ms | -86.1% |
| Join p95 | 41.389 ms | 5.400 ms | -87.0% |
| Join p99 | 44.932 ms | 5.494 ms | -87.8% |
| Entire bundle p50 | 77.325 ms | 16.427 ms | -78.8% |
| Entire bundle p95 | 83.857 ms | 17.198 ms | -79.5% |
| Entire bundle p99 | 89.170 ms | 17.694 ms | -80.2% |

Cgroup memory includes memory and file cache charged to the server group. PSS is the proportional physical memory of server processes, accounting for shared mappings. These are different accounting measures; summed RSS would double-count PostgreSQL's shared memory and is not used for comparisons. Cgroup memory is sampled every 50 ms, PSS every 500 ms; short-lived PostgreSQL workers can disappear during a PSS sample, and incomplete samples are recorded. Memory.peak in the raw reports also includes setup and is not the figure above.

## Six-client throughput without pacing

- PostgreSQL 18.6: 356.3 statements/second; 196.07 ms server CPU per bundle.
- Rust SQL: 4047.9 statements/second; 15.69 ms server CPU per bundle.

Rust SQL delivers 11.36x throughput at this concurrency. This is not an assertion of maximum server capacity at every client count. Clients use a median 27.8% of their two-core budget in the Rust SQL unpaced trials, so they did not saturate their CPU allocation.

## Correctness and remaining qualification

All 20 full trials passed: 91,200 timed SQL statements produced the expected values and PostgreSQL type IDs. All committed account balances matched the independent expected state before and after a normal server restart in every trial. There were no SQL failures, OOM events, memory-limit events, swap use, server/client CPU throttles, or remaining server processes. All 40 normal shutdowns returned zero. The additional two short pilot trials are retained but excluded from these medians. Fourteen existing resource-evidence unit tests also passed after the harness extension.

This benchmark does not establish crash durability, replication/failover correctness, long-running maintenance cost, or production reliability. Those require their own qualification. The previously reproduced 1 GiB spill-pressure cancellation remains unresolved; this comparison uses the published runtime, before the experimental pool-headroom change. Analytical relation-size function outputs are retained but not promoted into a disk-space claim because the engine-specific accounting has not been independently reconciled here.

## Reproduction and evidence

`evidence.tar.gz` contains the complete JSON reports, frozen Python harness, controller commands, effective settings, query plans, and cluster logs/configurations. `manifest.json` records each member and archive SHA-256. `results.json` contains medians and per-trial ranges. The absolute binary locations in commands refer to this workstation; use binaries with the recorded hashes to reproduce this exact comparison. The systemd driver requires a delegated private unit and does not alter global host memory settings.

- Native PostgreSQL binary SHA-256: `07b16e1aa4f0e2206c37b339b6e4cc813aa8ab9e3c5516334f753b3d90d0c536`.
- Rust SQL binary SHA-256: `5c1049ca3dd5d17fb91c7cca088808c4afce8236f62fa1807bcd094d7696555a`, the retained published runtime used by the preceding qualification.

Run `python3 replay.py` to verify archive integrity and reproduce all medians/ranges without starting either database. These reports describe the recorded frozen binaries; subsequent runtime or configuration changes require fresh measurements.

# Production build-profile screen

**Not selected.** The existing `dist` profile improves medium-load read/update p99 in this screen, but capacity update/aggregate p99 cross the provisional 5% preservation guard. Runtime source, allocator defaults and the retained executable remain unchanged. No new production qualification follows from this experiment.

The retained executable uses `release` with thin LTO. The candidate uses the existing `dist` profile, which inherits `release` and selects fat LTO; both use one codegen unit. The build command is `cargo build --locked --profile dist -j 2 -p main_main -p initdb`, with Rust 1.96.0. Its frozen source is `b2dfeea5200116fada0defa17db5299f13685b04`. The archive preserves the build command, successful log, profile configuration, source-equivalence witnesses and binary identities. Only a test-only Bloom-filter change differs from the constructor-fixed runtime source, apart from the embedded build identity.

| Binary | SHA-256 |
| --- | --- |
| Retained Rust SQL | `8e5bfc2b093055a5d2257dc817037a2b2f2906faf7c1b83e431f472a2f0513f0` |
| Dist candidate | `f0cf1dbe3f5df578e7e09835040f1b2abc777fd5fa8959c34e12047ba4e86c3f` |
| PostgreSQL 18.6 | `07b16e1aa4f0e2206c37b339b6e4cc813aa8ab9e3c5516334f753b3d90d0c536` |

The executable shrinks from 85,876,128 to 77,732,064 bytes, about 9.5%. This is file size; medium-load server memory does not improve. Eight SQL/protocol groups match PostgreSQL, including 192 committed updates per engine and normal shutdowns.

Each phase has five alternating three-arm repetitions: native PostgreSQL, retained Rust SQL and the candidate. Both Rust arms use the original allocator settings; every trial verifies an empty policy environment. The dataset has one million analytical rows, six ordinary-role clients, four server CPUs, separate client CPUs, a 2 GiB server limit and no swap. Durable commits, 16 MiB work memory, 128 MiB shared buffers and two parallel workers per gather remain enabled. Each client performs ten unpaced warmup bundles before measurement. Medium load measures thirty bundles per client at 9.4 bundles/second total; capacity measures fifty per client without pacing. Each bundle contains twelve point reads, four committed updates, one aggregate, one range scan and one join.

| Median metric | Medium retained | Medium dist | Change | Capacity retained | Capacity dist | Change |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| CPU ms/bundle | 19.945 | 19.579 | -1.83% | 19.096 | 19.080 | -0.09% |
| Statements/second | 179.38 | 179.40 | +0.01% | 3364.36 | 3330.66 | -1.00% |
| Charged memory MiB | 484.00 | 487.12 | +0.64% | 489.11 | 477.89 | -2.29% |
| Process PSS MiB | 510.62 | 511.68 | +0.21% | 516.59 | 501.97 | -2.83% |
| Point-read p99 ms | 0.65780 | 0.62105 | -5.59% | 4.86365 | 5.03101 | +3.44% |
| Committed-update p99 ms | 1.51321 | 1.42739 | -5.67% | 7.79764 | 8.21044 | **+5.29%** |
| Aggregate p99 ms | 5.78686 | 6.04360 | +4.44% | 14.94963 | 15.89201 | **+6.30%** |
| Range p99 ms | 1.52457 | 1.39562 | -8.46% | 6.86293 | 6.86548 | +0.04% |
| Join p99 ms | 7.88165 | 7.42839 | -5.75% | 19.74211 | 19.58561 | -0.79% |
| Bundle p99 ms | 21.00160 | 21.23473 | +1.11% | 49.98201 | 50.95800 | +1.95% |

The contemporaneous PostgreSQL medium control uses 241.653 ms CPU/bundle, 223.82 MiB charged memory and 95.93 MiB PSS, with read/update p99 of 0.48191/1.35175 ms. Its capacity throughput is 296.85 statements/second. These are fresh controls: absolute timings differ from earlier campaigns, so their percentages must not be compounded with previous results. All four target gaps remain open.

Including warmup, candidate CPU changes -1.67% at medium load and +1.18% at capacity; including dataset setup as well changes -1.87% and +1.60%. All Rust supplementary windows are unthrottled. PostgreSQL warmup throttling, where present, is retained separately in [results.json](results.json). These windows are separate accounting boundaries, not additive gains.

All thirty trials pass exact operation counts, result checks, committed-state checks across restart, normal shutdown and process cleanup. Timed server/client CPU has no throttling; there are no memory-limit events, OOM kills or swap. The capacity intervals for Rust are short, approximately 1.6-1.8 seconds per trial. These five-trial medians screen a candidate; they do not establish precise small-effect estimates or general server performance. Connection-startup, pressure, broad correctness and long holdout campaigns were not run for this unselected binary.

The [205-member archive](evidence.tar.gz) preserves every trial, cluster logs/configuration, harness sources, build/protocol evidence and the retained-runtime witness. `python3 replay.py` checks hashes, chronological arm order, limits, live policy, durability, CPU deltas, counts, cleanups, source witnesses and recomputed medians without extracting or executing archived contents. Replay passes on Linux/Python 3.12 and Windows/Python 3.14. Production readiness and the original memory/latency objectives remain open.

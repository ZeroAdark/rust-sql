# Portal and idle memory screening (2026-10-30)

The retained runtime and default settings are unchanged. The owned Bind-array candidate is rejected and its eight source/test files are restored. This report preserves failed experiments as well as a promising memory lead; it does not qualify a release.

CPU sampling of prepared point reads identified portal construction and transaction cleanup as useful investigation targets. A candidate gave the protocol parameter array explicit portal ownership, allowing empty contexts to reuse the existing pool. Its 46 unit tests pass, including repeated reuse, stale handles, live-allocation exclusion and abort cleanup. Eight groups of protocol checks match PostgreSQL for both the retained and candidate runtimes: prepared reads/updates under three plan-cache modes, large and null parameters, arrays/numeric, malformed binds, rollback, holdable cursors and plan invalidation. The retained runtime's 43 portal/parameter unit tests also pass. The new protocol probe and CI coverage are kept.

Five alternating three-engine repetitions per phase compare PostgreSQL 18.6, retained Rust SQL and the candidate with identical default policies. All 30 full trials pass correctness, committed-state/restart and resource checks: 136,800 timed statements plus 34,200 warmup statements, 30 restarts and 60 normal shutdowns. The short pilot is excluded.

| Candidate change against retained Rust SQL | Medium | Capacity |
| --- | ---: | ---: |
| CPU per bundle | -0.43% | +0.97% |
| Charged peak memory | +2.65% | -1.37% |
| Peak process PSS | +2.37% | -1.05% |
| Point-read p99 | +1.94% | +1.51% |
| Committed-update p99 | +0.34% | +1.50% |
| Aggregate p99 | +0.31% | +3.43% |
| Range p99 | -2.03% | +2.90% |
| Join p99 | -3.93% | +7.05% |
| Bundle p99 | +0.87% | +2.63% |
| Statements/second | Paced | -0.88% |

There is no consistent improvement in the four requested targets; capacity join p99 exceeds the provisional 5% preservation gate. These are observed median differences, not proof of statistical significance or a causal regression. The candidate is not promoted. Medium charged memory/PSS are 486.18/513.17 MiB against retained Rust SQL's 473.61/501.30 MiB and PostgreSQL's 223.86/93.03 MiB.

Two additional configuration screens each use three alternating three-engine repetitions at the same medium load. Both compare the exact same Rust executable with and without one explicit environment flag. All 18 trials pass correctness/restart/resource checks (61,560 timed and 20,520 warmup statements). Neither flag is selected as a new default.

| Screening configuration | CPU change | Charged peak change | PSS change | Status |
| --- | ---: | ---: | ---: | --- |
| PGRUST_POOL_IDLE_TAIL=1 | -0.01% | +1.21% | +1.01% | No demonstrated saving |
| PGRUST_NO_WORKER_POOL=1 | +0.39% | -10.77% | -10.01% | Memory lead; unqualified |

The idle-tail flag alone is not an effective intervention for these regularly paced sessions: the effective idle-release delay is 1000 ms, longer than the roughly 638 ms per-client bundle interval. The separate attribution profile deliberately waits 1300 ms. Do not attribute the screen's latency variation to active reclamation or claim the option cannot reclaim memory.

Disabling standby prewarming reduces medium charged memory/PSS from 499.77/527.26 to 445.93/474.50 MiB in its three-pair screen. Read/update p99 are 0.502/1.273 ms versus the retained control's 0.514/1.256 ms. Range p99 changes by +5.03%; the short screen is insufficient for promotion. More importantly, removing worker reuse can increase startup and CPU cost for heap queries using PostgreSQL-style parallel workers. Capacity and representative heap-parallel workloads still need evaluation.

A separate memory attribution run confirms 16 standby threads in the default process and none with the flag, while the four Gravity runtime workers remain. Startup anonymous memory is about 43 MiB lower, including 40 MiB fewer anonymous huge pages. This is a diagnostic observation, not an additional benchmark. It points to standby initialization and small-allocation page placement as further work, without sacrificing worker reuse prematurely.

Retained executable SHA-256: `8e5bfc2b093055a5d2257dc817037a2b2f2906faf7c1b83e431f472a2f0513f0`. Rejected executable: `cb27067b401c99f050e2b7d5e5e5a0f0440d51863beab9b8c3b04ce037c049ec`; its exact patch and sources are bound to parent `038a142250225354cca077de25b23709b0e03df5`. The symbolized diagnostic executable has byte-identical `.text` to the retained build; this does not assert whole-binary identity. Sampling is not latency/throughput evidence. Large raw perf captures remain local; their hashes and the reports/probe sources are archived.

Run `python3 replay.py` to verify all archived bytes, reproduce the summaries, and check all 48 full/screening trials. The archive includes the rejected patch, test/build logs, protocol controls, profiles, harness snapshots, settings, plans and server logs. Broader SQL/isolation/recovery suites were not rerun on the rejected executable. All four PostgreSQL targets remain open; the original published comparison remains the historical baseline. These short WSL2 trials do not establish representative Linux-server or production qualification.

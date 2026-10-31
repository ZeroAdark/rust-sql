# Retaining one recently used central batch

Rejected. All three prototype source files are restored to `b2dfeea5200116fada0defa17db5299f13685b04` in both checkouts, and the target executable is copied back from the retained `8e5bfc2b...` binary. This is a restoration, not a new build. Defaults remain unchanged.

The prototype keeps one ready batch for a recently active allocator size class while returning the remaining idle batches. The retained batch expires after the existing idle-drain period; pressure reclamation still drains everything. Bookkeeping uses one word per size class and adds no work to allocation/free paths. Returning blocks still takes linear work in the number returned. The new deterministic checks cover block/run lists, retained identity and zero status, reuse, expiry, timestamp wraparound and pressure bypass. Restoring the old full-drain behavior makes the retention check fail as expected. All 117 allocator tests pass (one existing helper ignored), the release build passes, and eight protocol groups match PostgreSQL under both default and fast settings, including 192 committed updates per engine per setting.

The medium-load campaign uses five alternating four-arm repetitions: native PostgreSQL 18.6, retained Rust defaults, retained Rust with the 200 ms policy, and the prototype with that same policy. The fast policy is `pool_warm_zero_max=16777216,pool_decay_ms=200`. Each trial verifies its live environment. All arms retain the established one-million-row, six-client, four-server-CPU, 2 GiB, no-swap, durable-commit design, with ten unpaced warmup bundles and thirty measured bundles per client at 9.4 bundles/second total.

| Median of five trials | Retained defaults | Retained + 200 ms | Prototype + 200 ms |
| --- | ---: | ---: | ---: |
| Server CPU per bundle | 17.676 ms | 17.726 ms | 17.748 ms |
| Peak charged memory | 489.82 MiB | 393.45 MiB | 399.61 MiB |
| Peak process PSS | 516.95 MiB | 418.66 MiB | 426.83 MiB |
| Point-read p99 | 0.48625 ms | 0.49599 ms | 0.52441 ms |
| Committed-update p99 | 1.31082 ms | 1.29304 ms | 1.29955 ms |
| Aggregate p99 | 4.32457 ms | 4.49730 ms | 4.95575 ms |
| Range p99 | 1.11628 ms | 1.15680 ms | 1.14075 ms |
| Join p99 | 5.67234 ms | 5.66495 ms | 5.84926 ms |
| Bundle p99 | 17.84126 ms | 18.80193 ms | 19.84488 ms |

Against retained defaults, the prototype saves 18.42% charged memory and 17.43% PSS but worsens read/aggregate/bundle p99 by 7.85%/14.60%/11.23%, failing the 5% preservation guard. Against the old policy alone, it uses slightly more memory and worsens those tails by 5.73%/10.19%/5.55%. Timed CPU changes +0.41% against defaults; including warmup changes -0.16%, and including setup as well changes -0.22%. These are separate windows, not additive gains. The result does not support accepting this code. No capacity or connection-burst campaign was run for the rejected prototype.

The archive also retains a separate fifteen-trial screen using the unchanged runtime with a 16 MiB ceiling and 500 ms decay. It passes the medium preservation guard but saves only 4.91% charged memory and 4.59% PSS, while worsening point-read p99 by 4.59%. It is not selected for a default change; no capacity or connection campaign was run for it. This is a weaker memory result than the 200 ms screen, not a failure of the predeclared 5% guard.

All 35 mixed-workload trials pass correctness, exact counts, committed-state checks before/after restart, normal shutdown and process cleanup, without timed CPU throttling, memory-limit events, OOM or swap. All Rust setup/warmup CPU windows are also unthrottled; some PostgreSQL unpaced warmups throttle and remain explicitly recorded. [results.json](results.json) retains both comparisons and supplementary CPU windows. Independently sampled charged-memory and PSS peaks use different accounting, and PostgreSQL sampling gaps remain recorded.

The [84-member archive](evidence.tar.gz) preserves 119,700 timed statements, every trial, frozen prototype source/patch, build/test logs, negative control, protocol results and restoration records. `python3 replay.py` verifies hashes, recomputes results and checks the evidence without extracting or executing archived contents. Replay passes on Linux/Python 3.12 and Windows/Python 3.14. All four PostgreSQL targets remain open; these short WSL x86-64 screens do not establish production qualification.

# Granular soft-budget reclaim: pressure and performance evidence

Candidate `635ec740` passes this screen and advances to runtime qualification. This archive does not promote it or establish production readiness. The comparison baseline is retained Rust SQL `0be60915`; PostgreSQL 18.6 is `07b16e1a`. All tests ran locally; no GitHub Actions were used.

The allocator now distinguishes an unknown budget from a known exhausted budget, holds optional cold-page prefaulting when headroom is low, and reclaims whole free superpages before growth. It preserves demand-allocation refusal semantics. The default budget path examines at most 64 bitmap words per state per search and returns at most eight 2 MiB superpages per call. Counter checks and fractional-debt accounting are O(1); CAS contention and OS operations prevent a strict wall-clock bound. The optional explicit ladder retains its previous reclaim path.

The preceding `f7f03377` candidate failed the 1 GiB range-p99 preservation guard. A focused diagnostic also demonstrated a 2 MiB return for a 4 KiB lazy commit. This candidate accumulates sub-superpage growth in thread-local accounting, releases whole amounts, and discards pending fractional debt when pressure clears. The diagnostic establishes over-reclamation; it does not by itself prove the cause of the earlier latency regression.

## Build and pressure checks

The build reports 181 passing tests (119 pagepool and 62 memruntime), with one ignored pagepool test. Three negative controls fail at the intended assertions: treating a known zero budget as unknown, removing the prefault hold, and restoring the preceding candidate's over-reclaiming purger. Live allocation byte patterns remain intact through the focused reclaim tests.

All six candidate pressure trials pass twelve alternating serial-spill/parallel sorts each. PostgreSQL passes all three trials. The retained Rust controls fail three trials: two kernel OOM group kills and one memory-watchdog cancellation. All twelve trials, including those failures, verify the complete ordered million-row digest after restart. The campaign contains 119 successful sorts. Candidate runs have no OOM kill, cancellation, swap use, leaked temporary files or failed restart check. Kernel memory-limit events still occur in these deliberately constrained 1 GiB trials; their timings are not performance evidence.

Three candidate trials include allocator ownership sampling and three omit it. Sampling changes the reproducer and is not treated as free observation. The observed candidate trials record nonzero inline reclamation. Cumulative returned bytes are not peak-memory savings.

## Performance preservation

Forty trials contain 1,812,600 timed SQL statements and 45,600 warmup statements. Each phase uses five interleaved repetitions per engine, one million rows, six clients, four physical server cores, two separate driver cores, hard CPU sets, no CFS quota, no swap, and enabled fsync/synchronous commit. Medium-load phases include PostgreSQL and both Rust builds; capacity compares the two Rust builds. SQL results, protocol type IDs, committed data, restart persistence, clean shutdown, limits and binary/source identities are checked.

Changes below compare the candidate with retained Rust, using medians of five trials. Negative CPU, memory and latency changes are improvements. All phases pass the predeclared 5% CPU/p99 guards; capacity throughput also passes its 5% guard.

| Phase | CPU per batch | Charged peak | Process PSS peak | Largest p99 increase | Throughput change |
|---|---:|---:|---:|---|---:|
| Medium, 1 GiB | -0.34% | -18.86% | -13.93% | Range +3.68% | Rate limited |
| Medium, 2 GiB | -0.14% | -3.04% | -2.67% | Point read +3.06% | Rate limited |
| Capacity, 2 GiB | -0.17% | -5.02% | -4.64% | Complete batch +0.15% | +0.22% |

CPU including setup, warmup, shutdown and restart also falls by 0.86%, 0.51% and 0.17%, respectively. These small differences are preservation evidence, not broad CPU-speedup claims.

At 2 GiB medium load, candidate/PostgreSQL charged peaks are 445.94/223.77 MiB and process PSS peaks are 473.55/92.71 MiB. Point-read p99 is 0.481/0.385 ms and committed-update p99 is 1.184/1.087 ms. CPU per mixed batch is 17.67/192.81 ms. The PostgreSQL memory and transactional-tail targets remain open. Charged memory and process PSS are separate accounting views with different page attribution and sampling; do not add them together.

## Reproduce the evidence review

Run `python3 replay.py` in this directory with assertions enabled. The verifier reads archived data without extracting or executing archived sources. It checks every member hash, source and binary identities, build/negative-control results, pressure outcomes, full restart digests, CPU binding, limits, exact workload counts, all three performance summaries and the retention decision. Deliberate changes to the binary, CPU binding, row count, restart, reclaim count and OOM evidence must be rejected. Replay passes on Windows and Linux.

The archive preserves the initial source-file ownership setup failure, which occurred before tests or a build started, and the subsequent successful build. Preliminary independent reviews remain marked incomplete; the final review reconstructs all forty trials. Raw failures and outliers remain in the evidence.

This is a short WSL x86-64 experiment on one PC. The mixed suite's analytical work dominates CPU cost. Connection startup, positive-budget sort checks and full SQL/protocol/recovery qualification are subsequent gates; longer soak tests, broader operational qualification and PostgreSQL parity remain required.

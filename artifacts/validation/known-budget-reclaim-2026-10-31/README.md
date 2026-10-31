# Known-zero reclaim budget: rejected latency screen

Candidate `f7f03377` is **not retained**. It fixes the known-zero versus unknown-budget ambiguity and passes the pressure reproducer, but the predeclared 1 GiB medium-load range-p99 guard fails. The retained executable remains `0be60915`; native PostgreSQL 18.6 is `07b16e1a`. No GitHub Actions were used.

The candidate publishes an explicit optional soft budget, holds optional cold-page prefaulting when that budget is exhausted, and reclaims whole free pages before growth. The default reclaim path examines at most 64 bitmap words per state per search and returns at most eight 2 MiB pages per call. Counter checks are O(1); CAS contention and OS remapping mean this is not a strict wall-clock bound. Demand-allocation refusal semantics and the explicit ladder's existing path are preserved.

The build reports 180 passing tests (118 pagepool, 62 memruntime), with one ignored pagepool test. Treating known zero as unknown and removing the prefault hold each make the focused test fail at the intended assertion.

All six candidate pressure trials pass 12 alternating serial-spill/parallel sorts each, with full million-row ordered digests, temp-file cleanup, clean shutdown and full restart verification. All three PostgreSQL trials pass; all three retained-build observer controls hit the memory watchdog. Across the 12 trials there are 119 successful sorts, three watchdog cancellations and no OOM kills. Rust's lifetime charged peaks still reach 1 GiB; cumulative bytes reclaimed are not peak memory savings. SQL observers affect allocator state, so observed and unobserved arms remain separate. Pressure timings include transport and digest work and are not performance-qualified.

The performance screen contains 40 trials and 1,812,600 timed SQL statements, plus 45,600 warmup statements. Each phase uses five interleaved repeats, one million rows, six clients, four physical server cores, separate driver cores, hard CPU sets, no CFS quota, zero swap and durable commits. All trials preserve SQL values/types and restart state, with no throttling, OOM or memory-limit events. Medium load is fixed at 9.4 bundles/second. Capacity uses 1,500 bundles per client and compares the two Rust binaries only.

| Candidate change versus retained Rust | 2 GiB medium | 2 GiB capacity | 1 GiB medium |
| --- | ---: | ---: | ---: |
| Server CPU per bundle | -0.85% | +0.09% | +0.60% |
| Throughput | Fixed rate | -0.23% | Fixed rate |
| Peak charged memory | -1.75% | -6.35% | -21.28% |
| Peak process PSS | -1.72% | -5.81% | -16.12% |
| Point-read p99 | +0.14% | +0.61% | -2.20% |
| Committed-update p99 | -2.70% | +3.72% | +3.93% |
| Aggregate p99 | -4.24% | +0.70% | -18.38% |
| Range p99 | +2.63% | +0.40% | **+10.39%: fails** |
| Join p99 | -6.15% | -0.56% | +2.13% |
| Bundle p99 | +3.26% | +1.01% | +4.25% |

At 1 GiB, charged memory falls from 403.18 to 317.37 MiB and process PSS from 411.04 to 344.77 MiB. Range p99 rises from 1.063338 to 1.173775 ms. PostgreSQL uses 223.76 MiB charged and 93.87 MiB PSS in that comparison, so memory parity remains open. The range median is similar, but higher tail latency requires investigation. Each medium trial has 180 range observations; these are empirical sample percentiles, not confidence bounds.

Including setup, warmup, shutdown and restart yields CPU changes of +0.42%, +0.14% and -0.45%, respectively. The measured CPU result does not hide a large deferred cleanup cost.

The original screen reused its phase-key variable in a resource loop and overwrote top-level phase summaries. The archive preserves that original report and controller. `results.json` reconstructs all three phases from the unchanged raw trials and agrees with each original per-phase review. Broader runtime qualification and positive-budget sort checks were prepared but not run because this preservation gate failed.

Run `python3 replay.py` to verify the archive, exact binary/source identities, unit and negative controls, pressure results, all 40 performance trials and the rejected decision. Replay rejects forged binary identity, CPU bindings, row counts, restart state and zero reclaim counters. It checks recorded evidence only; it neither executes archived code nor reruns SQL. Windows and Linux replay both passed. This experiment does not qualify a production release.

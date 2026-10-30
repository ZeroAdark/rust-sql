# Bounded small-run free-list experiment (2026-10-30)

**Rejected and reverted.** The candidate links unused allocator objects in roughly 4 KiB batches even on recycled 64 KiB runs, avoiding eager tail writes. Allocation and free fast paths stay unchanged. The new tail/payload test fails on the retained implementation and passes on the candidate; all 118 allocator tests pass, with one helper intentionally ignored. This confirms the intended behavior, not a workload-level resource saving.

Five alternating three-engine repetitions per phase compare native PostgreSQL, the retained Rust SQL constructor-fixed binary, and the candidate. Both Rust versions use unchanged default allocator settings. This isolates the code change from earlier reserve-policy experiments. All 30 full trials pass SQL-result, durable-update, restart and resource checks: 136,800 timed statements plus 34,200 warmup statements, 30 restarts and 60 normal shutdowns. The short three-engine pilot is retained separately.

| Candidate change versus retained Rust SQL | Medium | Capacity |
| --- | ---: | ---: |
| CPU per bundle | +0.70% | +0.30% |
| Charged peak memory | +1.68% | +1.74% |
| Peak summed PSS | +1.59% | +1.48% |
| Point-read p99 | -4.69% | +0.52% |
| Committed-update p99 | -2.86% | -3.82% |
| Aggregate p99 | +0.77% | -0.47% |
| Range p99 | -0.05% | +0.53% |
| Join p99 | +0.77% | +3.95% |
| Bundle p99 | -0.51% | +6.10% |
| Statements/second | Paced | -0.93% |

The candidate does not demonstrate a memory saving. Its capacity bundle p99 exceeds the provisional 5% guardrail, so it is not kept merely because some transactional latencies improve. Small percentage differences do not establish statistical significance; `results.json` retains ranges and the archive retains each paired observation.

At medium load the retained/candidate charged-memory medians are 477.48/485.52 MiB and PSS 504.57/512.61 MiB, against PostgreSQL's 223.80/94.11 MiB. The memory target remains open.

Retained Rust SQL SHA-256: `8e5bfc2b093055a5d2257dc817037a2b2f2906faf7c1b83e431f472a2f0513f0`. Rejected candidate: `ce85a3a3a2726ff57bc1a1d3594f5ee7a5a6d3fc1d3250bf9b7590a01c66cc83`. Its manifest binds the exact runtime patch and source to parent `db6830c3cea21408d458b9bc41c352bf75f1e2f6`. The archive includes the patch, source, positive and negative test logs, build log, harness snapshots, reports, settings, plans and server logs. It does not include executables.

Run `python3 replay.py` to verify every archived byte and reproduce the exact summaries and trial assertions. Broader SQL/isolation/recovery campaigns were not rerun for this rejected candidate. This experiment does not qualify a production release.

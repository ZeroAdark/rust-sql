# PostgreSQL comparison with a bounded warm reserve

This configuration experiment uses the constructor fix published in `db6830c3cea21408d458b9bc41c352bf75f1e2f6` and `PGRUST_MEMPOLICY=pool_warm_zero_max=16777216`. It does not change the default reserve policy. It repeats the original workload and limits with fresh clusters and five alternating PostgreSQL/Rust SQL pairs in each phase. The medium arrival rate stays at the original 9.4 bundles/second; it is not recalibrated to favor either engine.

| Metric | PostgreSQL | Rust SQL, 16 MiB reserve maximum |
| --- | ---: | ---: |
| Medium CPU per bundle | 192.480 ms | 17.561 ms |
| Medium peak charged memory | 223.80 MiB | 458.82 MiB |
| Medium peak summed PSS | 93.29 MiB | 485.06 MiB |
| Medium point-read p99 | 0.412 ms | 0.511 ms |
| Medium committed-update p99 | 1.165 ms | 1.264 ms |
| Medium aggregate p99 | 29.897 ms | 4.219 ms |
| Medium range p99 | 12.158 ms | 1.068 ms |
| Medium join p99 | 43.843 ms | 5.488 ms |
| Capacity SQL statements/second | 362.46 | 4,106.97 |
| Capacity CPU per bundle | 192.310 ms | 15.365 ms |
| Capacity peak charged memory | 232.52 MiB | 454.64 MiB |
| Capacity peak summed PSS | 100.18 MiB | 473.51 MiB |

Values are medians of five trials; latency values are medians of per-trial percentiles. `results.json` includes each metric's minimum and maximum. The prior Rust SQL medium medians were 479.77 MiB charged memory and 507.13 MiB PSS: this experiment is about 4.4% lower on both, still far above PostgreSQL. Read and update p99 remain slower than PostgreSQL.

The roughly 91% CPU saving and 11.3x capacity advantage over PostgreSQL remain. Against the earlier Rust SQL measurements, analytical p99 changes range from a 6.6% improvement to a 1.4% increase across these phases. Both engines consumed about 2% less CPU than in the earlier campaign, so this does not establish a causal CPU improvement from the policy. A directly interleaved Rust-default/Rust-policy comparison is still needed before changing defaults.

All 20 full trials passed result checks, committed-update checks before and after restart, 40 normal shutdowns, and the no-OOM/no-swap/no-throttling checks. There were 91,200 timed SQL statements plus 22,800 warmup statements. The short pilot is retained but excluded from aggregates.

Run `python3 replay.py` to verify the 143-member archive, source hashes, policy and executable identity, trial checks, and exact summaries. The archive contains raw reports, SQL plans, settings, server logs, immutable harness copies, command receipts and the constructor-fixed build manifest. Binaries are identified by SHA-256, not included.

This remains a narrow synthetic, compressible dataset on WSL2 Linux x86-64. It does not qualify production deployment, broad application compatibility, or other data distributions. Preserve the original comparison at `../postgresql-comparison-2026-10-30/`; these are additional observations, not replacement baseline numbers.

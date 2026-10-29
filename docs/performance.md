# Resource measurements

CPU savings are the primary goal. Bounded memory increases are acceptable when the CPU benefit is measured; correctness and latency remain release gates.

## Environment and method

2026-10-29: Ubuntu 24.04 on WSL2, Linux x86-64, Ryzen 7 9800X3D (16 logical CPUs visible), approximately 30 GiB RAM, ext4 build/data, Rust 1.96.0, locked release profile (thin LTO, one codegen unit). This is a development workstation measurement, not bare-metal server qualification. Windows load, virtualization, frequency changes and filesystem/cache state can affect results. No CPU pinning was applied.

Baseline: upstream `38634b408dea373f3944006dfbf74d19cbfc409f`. Candidate: the `cache.rs` and `pgsync/src/lib.rs` changes in this PR, built before the fork commit. Both binaries therefore identify the upstream build ID; use the retained binary SHA-256 values to distinguish them. Programs are preconstructed outside the cache timing windows. Seven repeated runs per binary, 50,000 operations per case. The two groups were measured sequentially, not interleaved. Treat the result as an algorithmic microbenchmark, not a complete SQL speedup claim.

## Local program-cache eviction pressure

Median nanoseconds per operation; a cyclic set of capacity + 1 keys forces one eviction per insertion.

| Live entries | Upstream | Candidate | Ratio |
| ---: | ---: | ---: | ---: |
| 128 | 227.16 | 77.19 | 2.94x |
| 512 | 720.26 | 77.24 | 9.33x |
| 2,048 | 2,707.54 | 77.89 | 34.76x |
| 8,192 | 10,762.15 | 82.31 | 130.75x |

Repeated hits on the newest key measured 23.77–24.09 ns upstream and 25.09–25.36 ns in the candidate, roughly 5–7% slower. The additional indexed load has a cost; eviction pressure is where this design wins. No per-hit allocations were introduced. These nanosecond wall times are not CPU-seconds-per-query measurements.

The allocator probe excludes prebuilt programs and allocator bookkeeping, and counts requested live heap bytes for the cache's metadata:

| Live entries | Upstream bytes | Candidate bytes | Increase |
| ---: | ---: | ---: | ---: |
| 1 | 212 | 340 | 128 bytes |
| 128 | 12,560 | 14,608 | 2,048 bytes |
| 512 | 50,192 | 58,384 | 8,192 bytes |
| 2,048 | 200,720 | 233,488 | 32,768 bytes |
| 8,192 | 802,832 | 933,904 | 131,072 bytes |

At the larger tested sizes the metadata increase is approximately 16.3%. This is a measured CPU/memory tradeoff, not a claim that RSS decreases. Vector capacity, hash-table slack, L1 ownership and later code promotion remain outside the L2 code-admission byte limit. Reset retains allocations for reuse.

## Complexity and limits

Local lookup/promotion/individual eviction are expected O(1), with amortized growth allocation. Evicting k entries takes O(k), replacing repeated O(k*n) victim searches. Prepared-plan bulk invalidation remains linear. Shared-cache byte accounting is O(1) per mutation and its census O(shards); shared eviction still scans its shard and k victims can cost O(k*s) for shard size s. Shared hits retain read-lock concurrency. Worker push/pop and periodic oldest-worker selection are O(1) through a deque; exceptional deregistration remains O(n).

Zero limits disable admission; oversized programs no longer flush the L2 or exceed its admission budget. Shared quota rounding no longer admits at least one entry in every shard when the requested global limit is below the shard count. These are bounded-admission fixes, not a process-memory cap.

## Reproduce

```sh
cargo run --release --locked -p gravity_program --example cache_pressure
cargo run --release --locked -p gravity_program --example cache_footprint
```

To compare upstream, copy only the example files into a separate checkout at the pinned baseline and build with the same toolchain/profile. Never compare a debug binary with a release binary.

The mixed probe uses four independent OLTP client processes (75% point reads, 25% committed updates across 10,000 accounts) and two OLAP client processes (32-group aggregation over a 9,900-row columnar table). It verifies every analytical result and the total number of committed increments. This small cached workload does not represent large joins, spilling, heavy contention or cache churn. The Python client/protocol overhead contributes to observed latency and throughput. CPU covers the supervisor and its server child; aggregate RSS double-counts shared pages and is not PSS/cgroup usage.

Five interleaved 10-second runs per binary, with reversed ordering on alternating pairs, produced these medians. Every run passed the 12 smoke checks, every analytical result matched, all committed increments were retained, and there were zero workload errors.

| Metric | Upstream | Candidate |
| --- | ---: | ---: |
| Server CPU microseconds / completed operation | 247.20 | 248.69 |
| OLTP operations / second | 9,611.58 | 9,635.70 |
| OLTP p99 milliseconds | 1.302 | 1.288 |
| OLAP operations / second | 2,122.85 | 2,127.13 |
| OLAP p99 milliseconds | 1.228 | 1.233 |
| Sampled peak summed RSS, MiB | 323.57 | 331.35 |

There is **no demonstrated end-to-end CPU improvement** in this small workload: measured CPU/op is about 0.6% higher. Throughput and tail-latency medians are close, and run-to-run variation is larger than their differences (upstream OLTP throughput ranges from 9,455 to 11,307 ops/s; candidate from 9,466 to 10,121). These results support further evaluation of the eviction optimization, not a database-wide performance claim or release acceptance. Peak aggregate RSS is about 2.4% higher by these medians. CPU per operation aggregates two query classes, so interpret it alongside their separate throughput figures.

```sh
sudo apt-get install python3-psycopg
ulimit -s 65520
python3 scripts/validation/compare_mixed.py \
  --baseline /path/to/upstream/bin --candidate /path/to/fork/bin \
  --work-dir /tmp/rustsql-checks --output-dir artifacts/mixed-run \
  --repeats 5 --seconds 10
```

Run on an otherwise idle machine, with both binaries already built. Reports include throughput, p50/p95/p99 latency, server CPU seconds, sampled peak aggregate RSS, errors and binary hashes. Every run creates a separate cluster; the harness keeps logs/data for inspection and refuses to overwrite per-run JSON evidence. Increase duration and dataset/concurrency coverage for release decisions. See [qualification gates](production-readiness.md).

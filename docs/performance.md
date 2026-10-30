# Resource measurements

CPU savings are the primary goal. Bounded memory increases are acceptable when the CPU benefit is measured; correctness and latency remain release gates.

## Aggregate validity forwarding, 2026-10-30

Forwarding recently loaded/stored aggregate validity bytes removes a measured
store-to-load dependency. The lookup examines at most 16 IR instructions (O(1)),
preserves every store and adds no per-row metadata. See the [implementation and
safety rules](aggregate-validity.md) and [complete retained evidence](../artifacts/validation/aggregate-validity-2026-10-30/README.md).

This comparison isolates that change against the preceding fork runtime, binary
`a5a76016a579d93da8e6cb73c95a419b7130ceba91c140bde6cdb017e2b0717b`.
The candidate is binary
`26ba69060488b50cc165b9a450c397eab524c1240870012d7c775e12f4540f77`,
built from `c25198bb144f1983db491c854fb8dfb5fa4dfd20` plus the two runtime source
files whose hashes are in the evidence manifest. It uses the same locked release
profile, fixed-work harness, guest CPU affinities, durability settings and dataset
as the earlier matrix. It is not a new direct comparison against upstream.

All 40 trials passed, checking 1,824,000 measured statements and all committed
updates again after restart. No workload errors, OOM, swap, memory-limit events
or CPU throttling occurred. Every one of the 20 paired CPU changes is negative
(2.48–6.20% lower). Medians across five trials per binary and cap:

| Cap | Previous CPU us/bundle | Candidate CPU us/bundle | Change in median CPU | Throughput change | Largest query p99 increase |
| --- | ---: | ---: | ---: | ---: | ---: |
| 1 GiB | 16,912.76 | 15,883.77 | -6.08% | +5.73% | none |
| 2 GiB | 16,312.60 | 15,591.00 | -4.42% | +4.06% | none |
| 4 GiB | 16,297.53 | 15,574.19 | -4.44% | +2.96% | +0.31% |
| 8 GiB | 16,321.20 | 15,556.86 | -4.68% | +3.45% | +1.22% |

The table compares the two medians. Median *paired* CPU changes are -4.37%,
-4.48%, -4.54% and -4.71%; these are different summaries, particularly at 1 GiB.
All pairs, ranges and query p50/p95/p99 values are retained. The provisional 5%
throughput/p99 regression limits pass at every cap. Five observations are not a
confidence interval, and this generated workload is not all database traffic.

Memory remains a tradeoff. Median sampled peaks, in MiB:

| Cap | Previous cgroup | Candidate cgroup | Previous PSS | Candidate PSS |
| --- | ---: | ---: | ---: | ---: |
| 1 GiB | 349.58 | 349.32 | 365.77 | 365.67 |
| 2 GiB | 587.68 | 601.42 | 612.26 | 628.21 |
| 4 GiB | 631.72 | 677.85 | 651.42 | 704.44 |
| 8 GiB | 701.48 | 691.40 | 713.18 | 715.78 |

At 4 GiB, cgroup peak rises 7.30% and PSS rises 8.14%; these costs are retained
despite the CPU benefit. No general memory saving is claimed. An earlier, separate
ten-trial 4 GiB calibration also reduced CPU in all five pairs, with a 4.74% change
in median CPU and a 6.30% PSS increase. It is excluded from the full matrix totals.
Profiling runs are also excluded from performance results.

The candidate passes 642 lowering tests, 124 validation tests, 88 focused aggregate
comparisons, 87 broader differential checks, seven EXPLAIN checks, the 142-case
window trace probe and 220 independent window checks. A fresh regression capture
accepts 231/231 under the reviewed contract (raw 203/231) and all 510 isolation
cases pass. This establishes a measured CPU improvement on this WSL2 workstation;
representative Linux-server workloads, overload, larger spills and long soaks
remain qualification gates.

## Earlier fixed-work resource matrix, 2026-10-30

The pre-forwarding runtime (`faf62fc4a544b2d033d812b9728b54ecfcf390ad`, binary
`a5a76016a579d93da8e6cb73c95a419b7130ceba91c140bde6cdb017e2b0717b`)
was compared with upstream `38634b408dea373f3944006dfbf74d19cbfc409f`
(`bd9c8954d1c58ac3c0dcea5035af7cf44fc7c0e4bf8acb243a034132781d35e3`).
This is a newer candidate and a different workload from the earlier measurements
below. Do not combine their numbers.

Forty interleaved trials cover five repetitions per binary at each of 1/2/4/8 GiB,
with four server CPUs, six ordinary-role clients and one million columnar fact
rows. A fixed bundle contains twelve indexed reads, four committed updates and
three analytical queries. Every trial checks all results and protocol types, then
checks every changed account again after restart. All 1,824,000 measured statements
passed; there were no workload errors, OOM events, swap use, memory-limit events
or CPU throttling. A separate small-process control verifies that the accounting
detects an intentional OOM and includes exited child CPU while excluding clients.

The host is the same WSL2 workstation described below. Server affinity was
0/2/4/6 and client affinity 8/10, with separate cgroup limits. These are guest CPU
affinities, not exclusive reservations of the Windows host. This remains
development evidence. See the [full method and commands](resource-benchmark.md)
and [retained reports](../artifacts/validation/resource-matrix-2026-10-30/README.md).

Medians across five trials per binary and limit; negative CPU changes are better:

| Memory cap | Upstream CPU us/bundle | Fork CPU us/bundle | CPU change | Throughput change | Largest per-query p99 increase |
| --- | ---: | ---: | ---: | ---: | ---: |
| 1 GiB | 16,835.42 | 16,851.73 | +0.10% | -0.02% | +3.52% |
| 2 GiB | 16,253.54 | 16,243.41 | -0.06% | -0.30% | +2.18% |
| 4 GiB | 16,265.60 | 16,270.87 | +0.03% | -0.49% | +0.75% |
| 8 GiB | 16,180.33 | 16,334.82 | +0.95% | -1.10% | +2.49% |

**This earlier comparison did not demonstrate an end-to-end CPU saving.** The stable prepared workload does
not exercise the LRU eviction improvement. At 8 GiB, all five paired CPU changes
are increases (0.006% to 1.645%); retain that cost in further investigation. The
provisional median throughput and query-specific p99 limits pass at all four
caps; CPU improvement was still open at this checkpoint. Server qualification remains open.
Five observations are not a confidence interval. Full paired values and ranges
are retained rather than hiding variation behind these medians.

Median sampled peaks, in MiB; charged memory and PSS measure different things:

| Cap | Upstream cgroup | Fork cgroup | Upstream PSS | Fork PSS |
| --- | ---: | ---: | ---: | ---: |
| 1 GiB | 349.54 | 349.37 | 357.20 | 372.44 |
| 2 GiB | 585.57 | 607.78 | 608.18 | 628.15 |
| 4 GiB | 657.32 | 655.64 | 681.34 | 675.88 |
| 8 GiB | 681.86 | 652.76 | 707.94 | 675.64 |

RSS, whole-trial cgroup peaks, user/system CPU, throttling and every query's
p50/p95/p99 remain in the raw reports. This dataset fits below the caps: it does
not establish behavior under overload, large spills or sustained maintenance.

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

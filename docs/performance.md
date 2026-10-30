# Resource measurements

CPU savings are the primary goal. Bounded memory increases are acceptable when the CPU benefit is measured; correctness and latency remain release gates.

## Transaction admission, 2026-10-30

The [admission fixes](transaction-admission.md) enforce a limit that extended-protocol queries previously bypassed. The earlier fixed-work runs configured `max_active_queries=4`, but that setting did not constrain those queries. Both sides of the earlier validity-forwarding comparison shared the defect, so its CPU comparison remains useful; it does not demonstrate working admission.

Two separate 4 GiB calibrations compare the preceding optimized binary `26ba69060488b50cc165b9a450c397eab524c1240870012d7c775e12f4540f77` with the corrected binary `e7d3f26e5f0acfb3de5bd9b0971a4168cbfca2d3a25407c64658721c9e4ac20a`. Each uses five interleaved pairs, six clients, one million fact rows, 400 measured bundles per client, 50 warmup bundles, four server CPUs and separate client CPUs. Both binaries receive the same configured limit in each experiment. Every result and all committed updates are checked, including after restart.

| Configured transaction limit | CPU us/bundle, previous / corrected | CPU change | Throughput change | Point-read p99 change | Largest query p99 increase | Peak PSS change |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| 4 | 15,552.73 / 15,529.13 | -0.15% | -4.28% | +14.94% | +14.94% | +9.22% |
| 6 | 15,572.39 / 15,544.83 | -0.18% | +0.19% | +0.63% | +3.07% | -2.52% |

These are changes in medians, not new demonstrated CPU savings: paired CPU changes straddle zero. Enforcing four slots increases point-read p99 from 4.25 to 4.89 ms, while analytical p99 falls 15–20%; this configuration **fails** the provisional per-query 5% latency gate. With six slots, matching the six clients' maximum concurrency, both throughput and every query p99 remain inside that gate. This supports using six as a starting point for this workload only; it is not a universal production setting or a claim about saturation beyond six clients. The server default remains zero (disabled).

All 20 trials pass correctness and restart checks: 912,000 measured statements and 192,000 measured committed updates, plus warmup. There are no OOM, swap, memory-limit or CPU-throttling events. The failed four-slot latency result is retained alongside the six-slot result in the [evidence package](../artifacts/validation/transaction-admission-2026-10-30/README.md). These are WSL development measurements at one memory cap; a new complete memory matrix and Linux-server qualification remain open. The benchmark now accepts `--max-active-queries` (default 4), records the value, and rejects individual trials that used different settings.

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

## Allocator selection (2026-10-30)

Keep Hearth as the default for the current CPU-first mixed workload. Selecting mimalloc reduces memory substantially, but fails the throughput and latency gates and increases CPU cost in every paired run. The runtime binary and default allocator are unchanged by this investigation.

Both sides use binary SHA-256 `2f35e3a361d2b660a73c5aefb739af6178ee3f4dfbd5b9b63f48b165e5d5ec3c`. Five interleaved pairs at 4 GiB use four server CPUs, separate client CPUs, six ordinary-role clients, six active transactions, one million fact rows, and 400 measured plus 50 warmup bundles per client. The allocator is explicitly selected and verified with `SHOW pgrust.allocator` before measurement and after restart. All 456,000 measured statements and 96,000 measured committed updates pass; restart checks include the 12,000 warmup updates too.

| Metric, median across five runs | Hearth | mimalloc | Change |
| --- | ---: | ---: | ---: |
| Server CPU microseconds / bundle | 15,461.77 | 16,111.17 | +4.20% |
| Bundles / second | 239.21 | 213.72 | -10.66% |
| Sampled peak server cgroup memory, MiB | 660.13 | 329.23 | -50.13% |
| Sampled peak summed RSS, MiB | 709.28 | 435.52 | -38.60% |
| Sampled peak summed PSS, MiB | 683.30 | 348.27 | -49.03% |

Paired CPU increases range from 3.69% to 4.99%. Changes in median query p99 are read +3.33%, update +3.26%, aggregate +12.16%, range +4.99%, join +10.80%, and whole bundle +9.05%. No memory-limit, OOM or swap events occur. The first mimalloc measurement records 1,171 microseconds of CPU throttling; the other nine record zero. These are observed distributions from a short WSL calibration, not confidence intervals or a complete 1/2/4/8 GiB matrix.

The separate 800-startup pilot with mimalloc passes under a 2 GiB cap at a 1,073,188,864-byte peak (1,023.47 MiB). The same connection configuration with Hearth had failed at 2 GiB. This capacity result does not outweigh the mixed-workload CPU and latency regression. An earlier environment-only pilot did not actually select mimalloc because the cluster helper strips inherited `PG*` variables; its Hearth log and failed verdict are retained as an invalid allocator control, not included in the comparison. Explicit cluster selections now survive that isolation and are checked against the running server. Validator tests reject missing or wrong allocator witnesses, changes after restart, and comparisons that change both binary and allocator.

A separate read-only residency probe captures two held 800-startup phases with Hearth. Most resident memory is concentrated in three anonymous 1 GiB mappings separated by 2 MiB guards, consistent with the pool layout in `pagepool/src/reserve.rs`; this is an inference from the mappings and source, not a complete allocation attribution. The capture adds diagnostic overhead and is not a performance trial. Targeted pool memory work, a complete matrix and Linux-server qualification remain open. [Raw reports, sources, logs, residency captures and replay instructions](../artifacts/validation/allocator-selection-2026-10-30/README.md) preserve both the memory benefit and performance failure.

## Sparse small-run policy (2026-10-30)

**Rejected; the runtime remains unchanged.** An experimental Hearth change advised base pages for general small-object runs before their first free-list write, reapplied the advice on reuse, and restored the pool policy on return. It kept the per-object allocation/free paths unchanged, but added kernel calls on run acquisition and return. Linux documents how a sparse write into a huge-page mapping can populate substantially more memory than the application touched. [Kernel THP documentation](https://docs.kernel.org/admin-guide/mm/transhuge.html)

The cold-run test fails on the original code. A control applying advice only after the first write also fails its residency assertion; the complete experiment passes, including content preservation and policy restoration. Its normal allocator suite passes 113 tests with one helper ignored by design.

Five interleaved mixed-workload pairs at 4 GiB, with six clients and `max_active_queries=6`, show effectively unchanged median CPU per bundle (-0.0018%), throughput +0.37%, and all median query p99 changes within the 5% limits. All ten trials pass their result, committed-update and clean-restart checks (456,000 measured statements). Sampled peak cgroup memory changes by -2.75% by the medians, but the direction varies across pairs. This does not demonstrate a CPU improvement or a consistent general memory reduction.

A separate comparison holds both binaries at 4 GiB for five interleaved pairs of two 800-connection waves. It verifies 16,000 accepted startups and 20 overflow rejections. Server CPU per accepted startup rises from 1,657.875 to 2,026.376 microseconds by the medians (**+22.23%**); every pair increases (+3.59% to +30.31%). Median first-wave completion time rises 146.09%; this includes client orchestration and diagnostic snapshots, so it is not standalone authentication latency. Peak cgroup memory changes only -1.65% by the medians. The CPU regression rejects the change despite the steady-workload result.

Three separate 2 GiB candidate runs complete both waves without memory-limit events, swap or leftover processes. Peaks are 1,985.25, 1,998.93 and 2,029.09 MiB, leaving as little as 18.91 MiB of headroom. A fresh baseline control is OOM-killed before completing a wave. These candidate pilots do not qualify the current runtime at 2 GiB: the candidate was rejected, and even its idle-connection headroom is insufficient to claim a safe active-query operating limit.

The retained change repairs the frame-zone test's measurement boundary: it verifies the returned pages are resident, then counts faults during touches rather than including first-touch allocation bookkeeping. The original assertion also fails on the unchanged baseline. The retained source passes all 112 normal allocator tests (one helper ignored), and the focused frame test now runs in Linux CI. Full SQL/isolation/crash campaigns were not repeated for the rejected binary. [Evidence, source patch, controls and portable replay](../artifacts/validation/allocator-small-pages-2026-10-30/README.md) retain both the successful pilots and the CPU failure.

Within a delegated service configured as described in [connection admission](connection-queue.md), reproduce the calibration with:

```sh
python3 scripts/validation/resource_workload.py \
  --baseline /path/to/fork/bin --candidate /path/to/fork/bin \
  --baseline-allocator hearth --candidate-allocator mimalloc \
  --work-dir /tmp/rustsql-checks --output-dir artifacts/allocator-run \
  --memory-mib 4096 --server-cpus 0 2 4 6 --client-cpus 8 10 \
  --clients 6 --max-active-queries 6 --rows 1000000 \
  --bundles 400 --warmup 50 --repeats 5
python3 scripts/validation/compare_resource.py --calibration \
  --input-dir artifacts/allocator-run --output artifacts/allocator-comparison.json
```

`--calibration` explicitly labels the 4 GiB scope. Without it, the comparison still requires the complete four-limit matrix. An allocator comparison requires one identical binary and two explicit distinct allocator choices; a binary comparison requires distinct binaries and identical explicit allocator choices, if any. No failed or refused query counts as useful throughput.

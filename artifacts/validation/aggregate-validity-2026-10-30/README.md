# Aggregate validity forwarding: 2026-10-30

All 20 paired comparisons use less server CPU with the change. Across the four
memory caps, the change in median CPU per fixed bundle is -4.42% to -6.08%;
throughput improves 2.96–5.73%. The largest per-query p99 increase is 1.22%.
Memory is mixed: at 4 GiB, sampled peak cgroup memory rises 7.30% and PSS rises
8.14%. These are Linux-on-WSL development results, not production qualification.

The candidate is built from `c25198bb144f1983db491c854fb8dfb5fa4dfd20` plus the two
runtime files recorded in `manifest.json`. Candidate PostgreSQL binary SHA-256:
`26ba69060488b50cc165b9a450c397eab524c1240870012d7c775e12f4540f77`.
The comparison baseline is the preceding fork binary
`a5a76016a579d93da8e6cb73c95a419b7130ceba91c140bde6cdb017e2b0717b`.
It isolates this optimization; it is not a direct comparison with upstream.

The locked release build uses Rust 1.96.0 and the existing profile, with no new
compiler flags. The runtime change preserves all validity stores and forwards
known bytes only within one basic block, with an O(1) bounded lookup. See the
[safety rules](../../../docs/aggregate-validity.md), [full metrics](../../../docs/performance.md)
and [measurement method](../../../docs/resource-benchmark.md).

Forty interleaved trials cover five repetitions at each of 1/2/4/8 GiB, four server
CPUs and six ordinary-role clients. Every trial executes 45,600 checked measured
statements, including 9,600 committed updates; all updates including warmup are
verified after restart. All 1,824,000 measured statements pass. No workload errors,
OOM, swap, memory-limit events or CPU throttling occur. The provisional 5%
throughput/p99 regression limits pass at every cap. The separate 4 GiB calibration
contains ten trials and is excluded from these totals. Profiling runs are also
excluded from performance measurements.

`comparison.json` retains every paired value, range, query percentile and sampled
memory metric. Its median CPU changes compare two medians; median paired CPU
changes are -4.37%, -4.48%, -4.54% and -4.71%. All 20 individual CPU changes range
from -2.48% to -6.20%. Five observations do not establish a confidence interval.

The new binary passes 642 lowering tests, 124 validation tests, 88 focused
aggregate comparisons, 87 broader differential checks and seven EXPLAIN checks.
The window trace probe passes 142 checks per engine against unmodified PostgreSQL;
a separate run against the patched reference is also retained. The independent
window oracle passes 220 checks per engine against the declared PostgreSQL patch.
The 12 smoke checks include committed writes, rollback, invalidation and restart.
All 510 isolation cases pass. The fresh reviewed regression audit accepts 231/231;
the raw PostgreSQL verdict remains failed, with 203/231 matching. No compatibility
profile or expected result was changed for this optimization.

`codegen.json` records the validity-byte loads in the captured group and join
modules. The group-only producer drops from 15 to six such loads across its three
row bodies; the corresponding join producers drop from 45 to 18 and 30 to 12.
The machine code confirms the targeted reload is replaced by a register read.
This explains the change, but instruction counts and sampling percentages are
not substituted for the unprofiled CPU measurements.

`evidence.tar.gz` contains 1,705 files: all matrix and calibration reports and
cluster logs, source snapshots, build/test logs, full raw regression and isolation
outputs, the existing native regression control and outputs, the new reviewed
audit, live probe reports, and before/after JIT maps, sampled IPs, IR, machine code
and disassembly. Large raw `perf.data` files remain local; their paths, sizes and
SHA-256 values are recorded explicitly in the manifest. The archive is 6,314,915
bytes, SHA-256
`38bbc72d89b4484a94b302d212b5d328e943f36131cbf13cb5cd75b6199002b2`.
Every archived member was read back and checked against its recorded hash.

After extracting to a fresh directory, reproduce the comparison with:

```sh
python3 scripts/validation/compare_resource.py \
  --input-dir /absolute/extracted/matrix \
  --source-dir /absolute/extracted/source/scripts/validation \
  --output /absolute/new-comparison.json
```

The native/candidate paths inside original capture reports identify the retained
local runs. Re-running the SQL probes requires freshly built binaries and the
ordinary-user, private-cluster workflow documented in the repository. No servers
are deployed by this evidence package. Broader workloads, overload, sustained
maintenance, Linux-server measurements and the remaining
[qualification gates](../../../docs/production-readiness.md) stay open.

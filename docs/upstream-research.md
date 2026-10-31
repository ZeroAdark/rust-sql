# Upstream research and implementation map

Research date: 2026-10-29. Source baseline: `malisper/pgrust` main at `38634b408dea373f3944006dfbf74d19cbfc409f` (v0.4-beta). Findings distinguish public claims from code observed in this fork. No private source was accessed or inferred as available.

## What happened to the unreleased version?

The [October 22 release post](https://pgrust.com/blog/pgrust-v0-4-beta/) says previously private development is moving to public development. The release describes Gravity, broader JIT targets, columnar transactional support, adaptive memory allocation and JSON statistics. These are already part of our baseline, so importing the current source incorporates them. The post still labels the engine experimental and calls out memory limitations; its performance claims are not measurements of this fork.

| Public design | Source in this checkout | Fork decision |
| --- | --- | --- |
| Vectorized execution, fused operators, JIT | `crates/gravity/`, `crates/tpde/` | Retain; measure on x86-64 before changing code generation |
| Fair query scheduling and worker reuse | `crates/backend/executor/runtime/`, `crates/_support/pgsync/` | Preserve safety protocol; remove queue shifts |
| Unified memory accounting and policy | `crates/_support/memledger/`, `mempolicy/`, `mcx/`, `pagepool/` | Preserve ownership; harden cache admission and accounting |
| Transactional columnar storage | `crates/backend/access/pgrcolumnar/` | Exercise rollback, writes and recovery; no format changes |
| Predicate reuse and frequency admission | `crates/gravity/hosts/src/ops/scan/pgrc/cond_cache.rs`, `crates/gravity/hosts/src/tinylfu.rs` | Already implemented; no duplicate cache layer |
| Connection scaling and idle passivation | `docs/design/connection-scaling.md` | Existing design reports incomplete Linux fleet validation; keep that gate open |

## Lessons applied

The [October 29 Gravity article](https://pgrust.com/blog/programming-at-light-speed/) describes comparing generated code with specialized native implementations, then improving the compiler and algorithms. ZoneWalk, ZoneTopK and TwoPass use columnar metadata to avoid unnecessary work; late materialization delays string decoding, and run collapse reduces hash probes for adjacent equal groups. These implementations are already present under `crates/gravity/lowerv4/src/ops/sort/topn/`, `dict/late_render.rs` and `table/face.rs`. They are retained, not claimed as newly invented by this fork. Their benefits depend on selectivity, ordering, cardinality and planner choice; they do not imply constant-time full-table analytics.

The [query-engine article](https://pgrust.com/blog/how-we-made-postgres-hundreds-of-times-faster-the-query-engine/) explains batching, operator fusion and SIMD as ways to reduce per-row overhead and memory traffic. It also says the earlier release-wide benchmark comparison was removed pending refreshed measurements. We retain the engine and publish isolated, reproducible measurements instead of inheriting a headline speedup.

The [JIT article](https://pgrust.com/blog/jit-compiling-code-in-5-us/) describes direct code emission using stencils. It is background for compile overhead, not a justification to replace the newer Gravity backend with the article's miniature regex compiler.

The [v0.3 reliability post](https://pgrust.com/blog/pgrust-v0-3/) describes differential evaluation and several testing approaches. Our qualification process similarly separates targeted tests, PostgreSQL differential tests, isolation tests, recovery testing and workload measurements.

The [outage article](https://pgrust.com/blog/the-four-horsemen-behind-thousands-of-postgres-outages/) motivates work on memory, concurrency, planner mistakes and vacuum. Its discussion of 64-bit transaction IDs and undo logs describes exploration. Those storage changes are not shipped by this fork: they need format design, migration, recovery and replication proofs.

## Complexity policy

Prefer expected O(1) hash lookup and queue operations and O(log n) ordered operations when they fit the semantics. State the bound's variable and memory cost. O(n) scans remain appropriate for aggregate input, bulk invalidation, and bounded structures where indexing costs more than it saves. Sorting arbitrary rows remains O(n log n); joins may necessarily produce quadratic output. Do not promise O(1) queries by substituting a cache for correct execution.

The local program-cache patch removes repeated victim scans. The shared cache retains read-locked hits and a bounded linear victim scan; making every hit a writer to maintain recency could worsen contention. Its remaining multi-eviction cost is documented for follow-up measurement.

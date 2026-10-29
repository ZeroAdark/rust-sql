# Compatibility investigation: 2026-10-29

Target: Linux x86-64, PostgreSQL 18.6, mixed OLTP/OLAP. CPU remains the primary optimization target. This work fixes a demonstrated instrumentation defect and patches vulnerable dependencies; it makes no new end-to-end CPU-saving claim.

## Fixed defect

`EXPLAIN ANALYZE CREATE TABLE AS ... WITH NO DATA` previously reported unknown row counts for a plan that was known not to execute. It now supplies zero instrumentation and reports zero rows/loops (text output: never executed). Direct SQL and prepared EXECUTE both match native PostgreSQL. Both destination tables remain empty and a volatile sequence expression is not evaluated. The change only handles EXPLAIN's skipped-plan state; it does not add per-row execution probes or invented counters for other fused operators.

The saved before-probe fails precisely the two instrumentation comparisons; the patched probe passes them. The raw `select_into` fixture passes after this fix. Other changes in the number of raw failures can be peer-order variation and must not be credited to this fix.

## Remaining raw differences

Raw PostgreSQL output is still compared byte for byte. `audit_regress.py` is a diagnostic that elides complete EXPLAIN result tables to locate other differences; it cannot approve a query or turn the suite green. In particular, sorting an entire output file would hide errors, duplicate loss, ordered-result errors and window-frame differences.

| Cases examined | Evidence and disposition |
| --- | --- |
| `collate.icu.utf8`, `generated_virtual`, `incremental_sort`, `partition_aggregate`, `partition_join`, `plpgsql`, `portals`, `write_parallel` | In the examined run, only EXPLAIN tables differ. Physical engine layout and missing per-node counters are still observable API differences. |
| `explain`, `memoize`, `partition_prune` | Also return plan text through SQL helper functions; these survive the diagnostic's EXPLAIN-table elision. Engine metrics/plan compatibility remains incomplete. |
| `create_index`, `geometry`, `inet`, `join` | Examined row differences occur within unspecified ordering or equal sort-key groups, including Infinity/NaN peers. Do not require a particular tie order. |
| `groupingsets`, `select_distinct`, `union`, `updatable_views` | Examined rows move within unspecified output order. Ordered aggregate/group/join probes independently compare typed rows and duplicates. |
| `tsrf` | Grouping/set-returning output has unordered peers; DISTINCT ON without ORDER BY selects a different permitted representative. This does not justify ignoring ordered DISTINCT ON results. |
| `window`, `sqljson` | Peer choice affects row-number/last-value/array element order. Added deterministic total-order windows, peer RANGE/GROUPS frames, exclusions and ordered JSON probes. These are bounded coverage, not blanket approval of every fixture hunk. |
| `merge` | Plan wrappers and trigger/RETURNING emission order differ. Ordered final-state checks must remain strict; broader application MERGE cases remain a release gate. |
| `aggregates` | The fixture's deliberately NULL-returning parallel combine function produces a different answer when used by PostgreSQL workers. The engine uses a serial path here; the same aggregate matches native PostgreSQL with worker parallelism disabled. |
| `join_hash` | Helper queries read implementation-specific spill batch counts and EXPLAIN fields; batch growth differs and an unavailable field yields NULL. These metrics are not fabricated to match PostgreSQL. |
| `select_parallel` | Gather workers, worker counts, worker-specific error context and plan statistics differ. The primary SQL error remains intact. |
| `sysviews` | The internal `Caller tuples` allocator context is absent. Monitoring clients cannot assume PostgreSQL's allocator-context topology. |
| `equivclass`, `matview` | Additional raw failures in the paired baseline disappear on a later run; not counted as fixes. Preserve run-specific diffs rather than inferring correctness from aggregate counts. |

PostgreSQL documents that [output order requires ORDER BY](https://www.postgresql.org/docs/18/queries-order.html) and [EXPLAIN plans/statistics can vary](https://www.postgresql.org/docs/18/using-explain.html). Those facts guide query-specific investigation; they do not authorize universal normalization. The fork's existing [tie-ordering contract](conformance/tie-ordering.md) remains applicable.

## Isolation and semantic checks

The raw isolation mismatch `deadlock-parallel` depends on advisory locks acquired in PostgreSQL Gather workers being released at worker exit. This engine holds them in the leader until commit. The existing upstream overlay already documents that architectural difference. A native PostgreSQL control with `max_parallel_workers_per_gather=0` produces the overlay's expected output byte for byte. The extended engine schedule passes 510 cases once the fixture-required `en_US.utf8` locale is installed. Vendor expectations are never overwritten; raw and engine profiles are labeled separately.

The differential driver passes 88 checks: 17 named SQL cases, 64 seeded variations, direct/prepared skipped-plan instrumentation, empty tables, absence of volatile evaluation, serial aggregate behavior and the native isolation control. Typed values, SQLSTATEs and primary error messages are compared. Only the explicitly unordered UNION ALL case uses a duplicate-preserving multiset. This remains much smaller than an application/driver corpus.

## Dependency security

The original lockfile produced 22 vulnerability matches plus unsoundness warnings. Wasmtime 48.0.2 is a development/test dependency, not in the native server dependency graph. zstd 0.13.3 is reachable from the server's compression, columnar, WAL, parquet and backup paths.

All consumers now share patched minimums: Wasmtime 48.0.8 and zstd 0.14.3 (zstd-safe 8.2.0), with legacy zstd decoders still disabled. The locked graph passes cargo-audit 0.22.2 with zero vulnerabilities and zero warnings against the saved advisory database revision. Relevant advisories include [zstd context-reuse data disclosure](https://rustsec.org/advisories/RUSTSEC-2026-0391.html), [zstd-safe seekable memory corruption](https://rustsec.org/advisories/RUSTSEC-2026-0398.html), and [Wasmtime backend miscompilation](https://rustsec.org/advisories/RUSTSEC-2026-0379.html). Not every advisory's triggering feature is used by the server; a clean dependency audit also does not establish complete application security.

The backend oracle initially rejected `min(-0.0,+0.0)` because Wasm returned negative zero while Rust's interpreter returned positive zero. [Rust min/max permit either equal operand](https://doc.rust-lang.org/std/primitive.f64.html#method.min). The oracle now permits either **input bit pattern only for opposite signed-zero inputs to min/max**, at both float widths. All other operations and operands retain the strict comparator, including signed-zero-sensitive arithmetic and copysign. No execution code or SQL comparison is weakened.

## Recovery and resource evidence

Ten SIGKILL cycles terminate the entire owned server process group, with committed inserts/updates/deletes and an open transaction containing uncommitted writes plus DDL. Runs alternate explicit checkpoint/no explicit checkpoint. Ordered logical digests of both heap and columnar tables survive every restart. Native PostgreSQL 18.6 pg_dump/pg_restore also restores rows, access methods and ACLs into a fresh fork cluster. Ordinary-role write/escalation denials and post-timeout connection recovery pass.

The same probe passes in a systemd cgroup with MemoryMax=1 GiB, MemorySwapMax=0, CPUQuota=400%, and zero OOM events. Its small dataset has 968 surviving rows per table. This tests bounded recovery operation, not sustained overload, memory-pressure admission or a representative analytical working set. [systemd-run resource properties](https://www.freedesktop.org/software/systemd/man/latest/systemd-run.html) enforce the limit outside the database.

Retained machine-readable reports, binary/input hashes, raw diffs, audit database revision and test summaries are in [the evidence directory](../artifacts/validation/compatibility-2026-10-29/). Authentication/TLS, PITR, replication/failover, I/O faults, sanitizers/fuzzing, large mixed-load limits and a long soak remain open in [the qualification gates](production-readiness.md).

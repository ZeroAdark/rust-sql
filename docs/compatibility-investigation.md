# Compatibility investigation: 2026-10-29

Target: Linux x86-64, PostgreSQL 18.6, mixed OLTP/OLAP. CPU remains the primary optimization target. This work fixes a demonstrated instrumentation defect and patches vulnerable dependencies; it makes no new end-to-end CPU-saving claim.

## Approved compatibility contract

The fork must preserve SQL values, multiplicities, required ordering, transaction semantics, errors and wire protocol. Plans and statistics may describe this engine rather than PostgreSQL's implementation, provided they are truthful. Unavailable measurements are absent; a guessed PostgreSQL allocation or batch count is not a measurement. This contract was selected by the project owner on 2026-10-29.

The native PostgreSQL 18.6 control passes all 231 unmodified regression cases with the same locale and reference build. `compare_regression.py` permits only duplicate-preserving row permutations under an unambiguous SQL ordering contract. It preserves values, errors, arrays, JSON and complete EXPLAIN text. Unresolved sort expressions, LIMIT, multiline rows and ambiguous statements remain exact. Its 27 tests include missing/changed values, tie-group boundaries, byte/line-ending preservation, quoted SQL, COPY data and seeded mutation/search checks. The existing order classifier was moved unchanged to a standalone module; missing upstream comparator helpers are not silently substituted.

The comparator completely explains `inet`, `updatable_views` and `union` in the earlier run. Four focused native/candidate proofs additionally check geometric Infinity/NaN peers with every sort key projected, DISTINCT ON representative membership, and unordered JSON partition aggregates. These prove the named queries, not permission to normalize arbitrary geometric values or JSON arrays. Window coverage uses an independent frame oracle whose evaluation order is identified by row_number over the same window. ROWS/GROUPS frame selection is constant time; RANGE bounds use binary search; array checks cost their output size.

## Reviewed regression profile: 2026-10-30

The initial [explicit engine profiles](../regress/engine-profile/README.md) account for 35 direct EXPLAIN tables in nine cases. All are planner descriptions without ANALYZE measurements. They describe whole-input aggregation, grouping-set append paths, cursor support, join orientation and the absence of PostgreSQL Gather backend workers. SQL results outside those tables are preserved; no helper-function result is exempted. Source/expectation/statement hashes bind every exact replacement, and new output cannot update a profile automatically.

The full auditor checks all 231 scheduled cases, verifies the native control against pinned upstream fixtures, and permits only complete upstream alternatives for optional features/platforms. The initial nine plan profiles and strict ordering comparator accepted **215/231** cases on two independently initialized full runs. Raw PostgreSQL remained **203/231**. The tooling uses forward transcript scans, hash lookups, logarithmic source-line lookup and sorting of the small replacement list; it adds no server work.

The current review accepts **227/231** across three full runs, while raw PostgreSQL still reports **203/231**. Seventeen profiles cover 75 direct plans, 29 EXPLAIN-wrapper results, 18 scalar projections of unavailable hash metrics and one allocator observation. A closed policy binds entire source files, helper definitions and exact call sites. Unfiltered root counts cannot change/disappear; incremental-sort JSON counts also remain exact. The upstream helpers that already mask numbers have separate, exact format rules. Their placeholders do not establish measured cardinality. The independent paired probe checks eight typed queries and their actual root rows/loops, requires the intended Memoize/incremental-sort paths, and checks cursor allocation accounting and memory-view privileges on both engines.

Five query-specific witnesses now verify the original captured geometric peers, DISTINCT ON representatives and JSON partition arrays. They do not rely solely on passing a separate example: they check the actual tables from each full regression run. Point distance ties are limited to the named Infinity inputs. Circle ties preserve all three sort keys. Representative membership, group coverage, exact integer elements, duplicates, NULL partitions and per-partition consistency are mandatory. Eighty-five unit tests include malformed/stale policies, altered root counters, invalid choices, line endings and all 216 valid representative combinations. PostgreSQL documents the [choice of the first DISTINCT ON row](https://www.postgresql.org/docs/18/sql-select.html#SQL-DISTINCT) and [order-sensitive aggregation](https://www.postgresql.org/docs/18/functions-aggregate.html); the witnesses apply those rules only to the pinned queries.

The grouping-set witness computes the complete integer ORDER BY key from the projected nullable columns and preserves every row and aggregate value within each peer group. A separate Python oracle checks ten cases against native PostgreSQL and the fork with default, disabled and forced grouping rewrites (40 executions). It covers NULLs, empty input, duplicate grouping sets, HAVING, windows and set-returning projections, and verifies the actual selected plan families. Native PostgreSQL produces the same first-arm constant expression labels for equivalent UNION ALL plans; these labels do not mean that every output row has that constant value. The probe checks the actual ordering and values.

The filtered-diagnostic probe passes 22 typed data/counter checks per engine, including the full tenk input, small-budget windows, populated partitions, generic prepared queries and UPDATE routing. Its corruption control damages eight excluded heap files while the owned cluster is stopped. The selected healthy partition still returns two rows; both first- and second-level corrupted partition controls report XX001. Every file is restored byte for byte while stopped and all eleven rows survive restart. The hash probe replays the original 255-statement transaction: 26 data results match pinned upstream values, and 18 helper calls return truthful NULL metrics in the fork while their actual query values match native PostgreSQL. The whole-transaction cases include skew, underestimated cardinality, wide values, rescans and full joins.

The four unresolved cases are `aggregates`, `merge`, `select_parallel` and `window`. The full audit deliberately returns failure until every case is resolved. [The latest reports, proofs and raw output](../artifacts/validation/filtered-diagnostics-2026-10-30/README.md) distinguish reviewed and raw verdicts. The [prior checkpoint](../artifacts/validation/qualification-2026-10-30/README.md) remains unchanged.

## Truthful diagnostics and partition sizing

Removed simulated PostgreSQL sort methods/space, hash buckets/batches/memory, and tuplestore memory statistics, along with the incremental-sort replay used to manufacture batch counters. Actual row/loop, buffer and available operator counters remain. Seven server checks verify real root counts, omission of unavailable fields, and unchanged volatile-expression behavior with ANALYZE. The explicit MERGE isolation overlay adds three Gravity execution reports; all non-diagnostic output is byte-identical to the vendor fixture. Vendor files remain untouched.

Partition-bit lowering now caps large group estimates before multiplying by record width, preventing u64 overflow from selecting too few partitions. This is constant-time work per table opening, not per row. Tests cover host-derived L2 budgets, boundary estimates and maximal integers against a u128 oracle. Two old tests assumed the reference machine's cache behavior; their portable versions retain exact aggregate results and the advertised per-run record bound. All 637 lowering tests and the focused top-sort integration test pass.

A four-run ABBA EXPLAIN microbenchmark (5,000 warm queries per run) observed 1.32/1.34 server CPU seconds before and 1.37/1.36 after the diagnostics/partition patch, about 2.6% higher on this narrow case. This is not a CPU improvement and is too small a sample for a performance qualification. It precedes the catalog-race fix. Keep the raw samples and binary identities; ordinary OLTP/OLAP savings remain unproven.

## Catalog invalidation race

The parallel regression sweep reproduced `cache lookup failed for relation` during an operator-family alteration. The focused probe creates/drops indexes concurrently with family changes: native PostgreSQL completed 1,600 DDL statements, while the pre-fix fork failed at its fifth family change. The family scan returns unlocked relation OIDs; a concurrent DROP can remove a relation before its subsequent pg_class lookup.

The patch tolerates only a missing relation in that pre-commit invalidation walk. DROP already sends its own invalidation. Surviving relations retain correctly scoped messages; the ordinary strict lookup API and other catalog lookup errors are unchanged. The new regression test fails before the fix, and all 20 invalidation unit tests pass after it. The original 1,600-operation workload and an extended 16,000-operation workload both pass on native PostgreSQL and the patched fork. The raw `equivclass` fixture also passes after the fix. These are bounded concurrency tests, not a general absence-of-races proof.

## Corrected window semantics: explicit PostgreSQL bug difference

The initial window oracle passes 210/216 cases on both native PostgreSQL 18.6 and the diagnostics candidate. The six failures are two ROWS/EXCLUDE TIES frames over three shuffled seeds: first_value, last_value or nth_value returns an excluded/wrong row or NULL when the current row lies outside its frame. This matches the upstream [PostgreSQL bug report and pending fix](https://commitfest.postgresql.org/patch/7412/).

The project owner explicitly chose correct SQL behavior over retaining this PostgreSQL bug. The fork now skips overlapping peers when the current row lies outside the frame, instead of treating the excluded current row as an eligible replacement. The two added conditions are constant-time and add no per-frame scan or allocation. This deliberately changes the affected SQL values compared with unpatched PostgreSQL 18.6.

The independent oracle plus four minimal upstream examples passes all 220 cases on the corrected fork and on a separately built PostgreSQL control carrying the upstream v4 fix. The original native installation and vendor tree remain unchanged. Reports label patched controls and record binary and patch hashes; [patch provenance](../patches/postgresql/README.md) identifies the upstream author, pending status and source. No result masking is used to make the corrected oracle pass.

The combined final candidate passes the 16,000-operation catalog stress test, 88 differential checks, seven EXPLAIN checks, four scoped choice proofs, 220 window checks, 510 reviewed engine isolation cases and ten crash/restore cycles. Raw PostgreSQL regression remains 203/231: the corrected `equivclass` case passes, `subselect` now exposes truthful Gravity diagnostics, and unspecified ordering still changes raw counts. See the [exact final evidence and remaining strict differences](../artifacts/validation/conformance-2026-10-29/README.md). This is not production qualification or a claim that every raw mismatch has been adjudicated.

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

Retained machine-readable reports, binary/input hashes, raw diffs, audit database revision and test summaries are in [the evidence directory](../artifacts/validation/compatibility-2026-10-29/). The later [authentication/TLS probe](../artifacts/validation/qualification-2026-10-30/README.md) passes 21 bounded checks. Broader security/driver coverage, PITR, replication/failover, I/O faults, sanitizers/fuzzing, large mixed-load limits and a long soak remain open in [the qualification gates](production-readiness.md).

# Reviewed diagnostic expectations and SQL choice proofs

These nineteen profiles change 178 complete diagnostic tables under the owner's
SQL/protocol compatibility contract: 128 direct EXPLAIN tables, 30 plan-wrapper
results, 18 projections of unavailable hash metrics, one worker-counter observation
and one allocator inventory.
They do not replace vendor SQL or expected files. Measured root rows/loops cannot
change or disappear where the upstream helper retains them. Existing upstream
numeric placeholders are checked as formatting, never used as measurement evidence.

| Case | Tables | Reviewed difference |
| --- | ---: | --- |
| collate.icu.utf8 | 1 | Whole-input aggregation instead of partial/final aggregation |
| generated_virtual | 1 | Grouping sets use append branches, including false HAVING branches |
| portals | 2 | Cursor support without PostgreSQL Materialize wrappers |
| plpgsql | 1 | No PostgreSQL Gather backend worker for debug_parallel_query |
| write_parallel | 4 | Engine aggregation paths for CTAS, SELECT INTO and materialized views |
| partition_join | 2 | Global grouping over appended joins and engine parallel execution |
| select_distinct | 4 | Hash/sorted uniqueness paths without Gather workers |
| join | 2 | Semi-join orientation and nested join plan choices |
| partition_aggregate | 18 | Whole-input/partition aggregation and append paths |
| subselect | 1 | EXPLAIN-only wrapper: unavailable sort statistics and real Gravity diagnostics |
| memoize | 7 | One direct plan plus six wrapper results with unavailable internal counters |
| incremental_sort | 17 | Thirteen direct plans, two text wrappers and two JSON plan projections |
| sysviews | 1 | No PostgreSQL `Caller tuples` context in the engine cursor allocation path |
| groupingsets | 8 | Grouping-set union alternatives and their inherited first-arm expression labels |
| explain | 4 | One upstream-filtered JSON plan and three window-plan wrappers |
| partition_prune | 17 | Three direct plans, twelve parallel wrappers and two UPDATE wrappers |
| join_hash | 33 | Fifteen direct plans and eighteen NULL projections of unavailable batch measurements |
| aggregates | 6 | Serial aggregation and decorrelated plans; the inconsistent combiner fixture has a separate result witness |
| select_parallel | 49 | Forty-seven direct plans, one sort-statistics helper and one truthful worker-counter observation |

Each profile binds the whole SQL source, original expectation, statement occurrence
and original plan table by SHA-256. Replacements are exact aligned plan tables;
there are no regular-expression masks. The verifier rejects changed inputs,
non-EXPLAIN replacements, missing echoes, duplicate replacements and appended
statements. The separate [diagnostic policy](policy/diagnostics.json) binds the
complete source, twelve helper definitions and 38 unique diagnostic statements. A
profile cannot grant permission to arbitrary helpers or scalar queries. The only
allocator observation accepts an empty result; the companion live probe checks
cursor values, byte accounting and view permissions. Errors, notices and other
data remain strict. A profile never grows when new output is encountered.

The additional filtered-helper formats validate exact upstream root placeholders
and zeroed JSON counters throughout the plan. Hash metric replacements must be
exactly one row of SQL NULLs with the pinned columns; false, zero, missing rows or
other values are rejected. These scalar diagnostics are reported separately from
plan tables. The unfiltered companion checks actual typed values and root counts,
including the original tenk fixture, constrained windows, nonempty partition
queries, UPDATE routing and the 255-statement hash diagnostic transaction.

Owned-cluster corruption controls verify runtime pruning at both partition levels:
eight excluded heap files are corrupted while stopped, a prepared generic query
still returns the two healthy rows, selecting either corrupted partition fails
with XX001, and exact file restoration recovers all eleven rows. This is a bounded
read-corruption check, not a complete storage-failure campaign.

The [choice policy](policy/choices.json) admits five exact queries already covered
by live native/candidate probes. `choice_witness.py` proves their captured rows:
only the two named infinite-distance point peers may swap; circle peers retain
identical radius, point x coordinate and Infinity/NaN distance; DISTINCT ON keeps
one valid source representative per SRF group; and the named full-partition JSON
aggregate preserves exact integer elements, duplicates and one consistent array
per partition. The grouping-set witness permits only permutations within equal
`coalesce(a,0)+coalesce(b,0)` integer keys, preserving every aggregate value.
Finite geometric values never acquire ties from rounded output.
Unknown queries, changed elements, duplicate loss and missing rows fail.

`grouping_contract.py` independently calculates grouping results for NULL keys,
empty inputs, duplicate grouping sets, HAVING and windows. Ten cases run against
native PostgreSQL and each of the fork's default, off and forced rewrite modes.
The probe verifies that off/force selects the original/union plan family. Native
PostgreSQL also labels sorted UNION ALL columns using the first arm's expression
(even a constant); the captured plans use that convention, while actual values
and ordering remain checked.

The separate [aggregate strategy policy](policy/aggregate-strategy.json) covers
only the second `balk(hundred)` call in the pinned fixture. Its deliberately NULL
combiner does not satisfy PostgreSQL's partial-aggregation equivalence requirement.
`aggregate_strategy.py` requires a serial Aggregate plan and derives the exact
sum from the pinned tenk setup/data. It rejects wrong sums and serial NULLs,
including the NULL that happens to match the original parallel native output.
The first NULL-transition call, errors and all other values remain strict.
Seventeen live controls per engine cover serial/partial execution, NULL/empty
input, duplicates, groups and a valid sum combiner. Real native workers and
measured root rows/loops are required.

The [worker-context policy](policy/worker-context.json) permits only two absent
final PostgreSQL worker stack entries. The complete primary error and SQL-function
context stay exact, and statement occurrences prevent changes to the first serial
error. `parallel_contract_probe.py` checks SQLSTATE and the full 20,048-byte error
message over simple and extended protocols, role restoration, savepoint recovery,
30,000 typed sort/rescan rows and real worker counters (23 checks per engine).
Only the pinned `pg_stat_database` boolean observation may change to false/false,
reflecting zero PostgreSQL backend-worker launches; ordinary scalar data stays strict.

`scripts/validation/engine_regression_profile.py --record` is an explicit authoring
operation requiring a reason and a new file. It refuses to record a case if any
other difference remains after permitted row permutations. Review the complete
plan changes and verify the profile on an independent full run before committing.
The current profiles and witnesses match a subsequent fresh full run on Linux
x86-64 as well as the two earlier runs. The full
auditor first requires every native result to match a pinned PostgreSQL fixture;
whole-file upstream alternatives cover optional feature/platform branches.

The raw suite remains separate. The reviewed profile accepts 229/231 cases, with
two unresolved cases. This is not a complete compatibility or production claim.
See [the investigation](../../docs/compatibility-investigation.md) and
[retained evidence](../../artifacts/validation/parallel-contract-2026-10-30/README.md).

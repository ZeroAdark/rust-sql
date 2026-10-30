# Reviewed diagnostic expectations and SQL choice proofs

These thirteen profiles change 61 complete diagnostic tables under the owner's
SQL/protocol compatibility contract: 49 direct EXPLAIN tables, 11 results of
reviewed EXPLAIN wrappers, and one allocator inventory query. They do not replace
vendor SQL or expected files. Measured root rows/loops cannot change or disappear;
the two incremental-sort JSON projections also retain their measured counts.

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

Each profile binds the whole SQL source, original expectation, statement occurrence
and original plan table by SHA-256. Replacements are exact aligned plan tables;
there are no regular-expression masks. The verifier rejects changed inputs,
non-EXPLAIN replacements, missing echoes, duplicate replacements and appended
statements. The separate [diagnostic policy](policy/diagnostics.json) binds the
complete source, five helper definitions and twelve exact diagnostic calls. A
profile cannot grant permission to arbitrary helpers or scalar queries. The only
allocator observation accepts an empty result; the companion live probe checks
cursor values, byte accounting and view permissions. Errors, notices and other
data remain strict. A profile never grows when new output is encountered.

The [choice policy](policy/choices.json) admits four exact queries already covered
by live native/candidate probes. `choice_witness.py` proves their captured rows:
only the two named infinite-distance point peers may swap; circle peers retain
identical radius, point x coordinate and Infinity/NaN distance; DISTINCT ON keeps
one valid source representative per SRF group; and the named full-partition JSON
aggregate preserves exact integer elements, duplicates and one consistent array
per partition. Finite geometric values never acquire ties from rounded output.
Unknown queries, changed elements, duplicate loss and missing rows fail.

`scripts/validation/engine_regression_profile.py --record` is an explicit authoring
operation requiring a reason and a new file. It refuses to record a case if any
other difference remains after permitted row permutations. Review the complete
plan changes and verify the profile on an independent full run before committing.
The current profiles and witnesses match a subsequent fresh full run on Linux
x86-64 as well as the two earlier runs. The full
auditor first requires every native result to match a pinned PostgreSQL fixture;
whole-file upstream alternatives cover optional feature/platform branches.

The raw suite remains separate. The reviewed profile accepts 223/231 cases, with
eight unresolved cases. This is not a complete compatibility or production claim.
See [the investigation](../../docs/compatibility-investigation.md) and
[retained evidence](../../artifacts/validation/semantic-contract-2026-10-30/README.md).

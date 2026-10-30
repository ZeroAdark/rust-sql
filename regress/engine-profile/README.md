# Reviewed EXPLAIN expectations

These nine profiles change 35 complete, direct EXPLAIN tables under the owner's
SQL/protocol compatibility contract. They preserve every other output byte before
the separate, duplicate-preserving SQL ordering comparison. They do not replace
vendor SQL or expected files. No profile contains ANALYZE measurements.

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

Each profile binds the whole SQL source, original expectation, statement occurrence
and original plan table by SHA-256. Replacements are exact aligned plan tables;
there are no regular-expression masks. The verifier rejects changed inputs,
non-EXPLAIN replacements, missing echoes, duplicate replacements and appended
statements. Plan helpers, errors, notices, scalar values, arrays and JSON receive
no exemptions. A profile does not grow when a new candidate output is encountered.

`scripts/validation/engine_regression_profile.py --record` is an explicit authoring
operation requiring a reason and a new file. It refuses to record a case if any
other difference remains after permitted row permutations. Review the complete
plan changes and verify the profile on an independent full run before committing.
The current profiles match both retained full runs on Linux x86-64. The full
auditor first requires every native result to match a pinned PostgreSQL fixture;
whole-file upstream alternatives cover optional feature/platform branches.

The raw suite remains separate. The reviewed profile accepts 215/231 cases, with
16 unresolved cases. This is not a complete compatibility or production claim.
See [the investigation](../../docs/compatibility-investigation.md) and
[retained evidence](../../artifacts/validation/qualification-2026-10-30/README.md).

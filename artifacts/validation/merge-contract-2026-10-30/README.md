# MERGE action/trigger contract: 2026-10-30

The review accepts **230/231** cases on all three retained full runs. Raw
PostgreSQL output remains **203/231**. Only `window` remains unresolved, and every
full audit still returns failure.

The new profile contains eight source-bound EXPLAIN helper tables. Measured root
rows/loops and every displayed MERGE inserted/updated/deleted/skipped count must
remain exact. The fork reports uncounted internal nodes explicitly.

The one differing trigger/RETURNING block is checked against the pinned target
and source inputs. All four row actions, old/new values and returned values must
be correct. BEFORE/AFTER row notices and RETURNING rows must use one consistent
processing order. Statement-trigger phases remain exact. This is an O(n) proof
with hash lookups and O(n) storage, not independent sorting of notices.
PostgreSQL documents the [indeterminate default source-row order and MERGE trigger behavior](https://www.postgresql.org/docs/18/sql-merge.html),
and the [timing of row and statement triggers](https://www.postgresql.org/docs/18/trigger-definition.html).

The live probe passes **11 checks per engine**:

- Replays the first 664 original SQL source lines and requires byte-exact native
  expected output on both engines, including expected errors and permissions.
- Executes the original MERGE as `regress_merge_privs`; checks typed old/new
  composite values, command tag `MERGE 4`, the complete notice trace, final table
  contents and restoration of both tables on rollback.
- A duplicate source row raises SQLSTATE 21000; savepoint recovery proves target
  writes are atomic, and outer rollback restores the source.
- For each of eight helpers, independently calculates action counts and complete
  table contents. The original text helper runs first, then rolls back to a
  savepoint. An unfiltered JSON EXPLAIN runs the same operation once, and both
  executions must match the same expected state and measured counters.

**105 validation tests pass**, including all 24 consistent four-row processing
orders, wrong values/actions, independently reordered phases, missing/duplicate
rows, changed counters, stale source, malformed output and unrelated errors.
The live probe is wired into optional full-server CI; unit tests run on every PR.

Runtime source remains `faf62fc4a544b2d033d812b9728b54ecfcf390ad`; postgres SHA-256
is `a5a76016a579d93da8e6cb73c95a419b7130ceba91c140bde6cdb017e2b0717b`.
The preceding validation commit is `bfa79c0e7627e2c4190d1590eba9b4e660177d6e`.
This checkpoint changes validation/profiles/documentation and makes no new CPU
or production-readiness claim.

`engine-*.json` and `probe.json` are compact summaries. `raw-evidence.tar.xz`
contains the three original MERGE transcripts, complete per-case audits, full
probe plans, replayed setup transcripts and owned-cluster logs. `manifest.json`
hashes the archive, summaries, tools, policies, profiles and fixture inputs.
The preceding [parallel evidence](../parallel-contract-2026-10-30/README.md)
remains unchanged.

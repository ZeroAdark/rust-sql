# Aggregate strategy validation: 2026-10-30

The review accepts **228/231** cases on all three retained full runs. Raw
PostgreSQL output remains **203/231**. `merge`, `select_parallel` and `window`
remain unresolved, and every full audit still returns failure.

The `aggregates` fixture defines a sum transition with a combiner that always
returns NULL. That intentionally does not meet PostgreSQL's requirement that
combined partial states represent aggregation over the union of their inputs.
[CREATE AGGREGATE documents that contract](https://www.postgresql.org/docs/18/sql-createaggregate.html).
Native PostgreSQL itself returns 495000 in serial mode and NULL in partial mode
on the same pinned input. The fork uses serial aggregation and returns 495000.

The separate source-bound witness checks the complete SQL/native expectation,
the second occurrence of the query, the preceding serial Aggregate plan, and
the original tenk setup/data hashes. It calculates the input sum in linear time
with constant auxiliary storage. No arbitrary scalar substitution is permitted.
A serial NULL fails even though it matches the original parallel expected value.
The first NULL-transition test and all ordinary SQL values remain strict. The
new profile contains only six complete plan tables.

`aggregate_strategy_probe.py` passes **17 checks per engine** on Linux x86-64:

- Strict NULL transitions, empty input, NULL-only input, signed duplicates and
  grouped sums.
- The deliberately NULL-combining aggregate in serial and parallel-requested
  modes, with native partial/final nodes and real workers required.
- A valid sum combiner in both modes, which produces identical results on both
  engines. Typed values and actual root row/loop counters are checked throughout.

**92 validation tests pass**, including wrong sums, serial NULLs, partial plans,
the first query occurrence, changed source/input, malformed tables, line endings
and unrelated errors. The optional full-server workflow runs the new live probe;
the unit tests run on every PR.

The runtime remains the binary validated in the preceding checkpoints:
`faf62fc4a544b2d033d812b9728b54ecfcf390ad`, postgres SHA-256
`a5a76016a579d93da8e6cb73c95a419b7130ceba91c140bde6cdb017e2b0717b`.
The preceding published validation commit is
`dd79ac1bc4ac2ac6f9fba49f9f88de67f18050d3`.
This checkpoint changes validation, profiles and documentation; it makes no new
CPU or production-readiness claim.

`engine-*.json` and `probe.json` are compact summaries. `raw-evidence.tar.xz`
contains the three original aggregate transcripts, complete per-case audits,
full probe plans and owned-cluster logs. `manifest.json` hashes the archive,
summaries, validation sources, policies, profiles and fixture inputs. The earlier
[filtered diagnostics evidence](../filtered-diagnostics-2026-10-30/README.md)
remains unchanged.

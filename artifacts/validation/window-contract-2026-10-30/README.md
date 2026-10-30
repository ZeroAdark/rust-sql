# Window contract and complete regression review: 2026-10-30

The reviewed engine contract accepts **231/231** cases on four full captures,
including a new complete execution. Raw PostgreSQL output remains **203/231**;
the raw verdict, differences and outputs are retained separately. This closes
the remaining fixture mismatches under the agreed SQL/protocol contract. It is
not a claim of complete PostgreSQL compatibility or production qualification.

Twenty existing diagnostic profiles still cover 186 tables. No window plan
replacement was added. Instead, 24 source-bound window witnesses independently
prove the captured data results:

- Complete input multisets come from the pinned tenk data and original employee,
  NULL and datetime literals. Values, duplicates and NULLs remain mandatory.
- One consistent order determines first/last/nth values, lead/lag, frame bounds,
  exclusions, ranks and peer counts. Equal PARTITION/ORDER clauses share that
  order. No independent per-cell choice is accepted.
- Filtered partition representatives must cover every eligible partition exactly
  once with correct counts and ntile. Opposite-order rank filters construct two
  complete orders, including omitted rows, then recompute the full predicate.
- ROWS/GROUPS bounds and first/last/nth selection take O(1) per row. RANGE bounds
  use binary search. Full verification is O(n log n) worst case, O(n) storage;
  the hash-based membership passes are expected O(n). No permutation search is
  used by the verifier.

PostgreSQL documents [frame/ranking semantics](https://www.postgresql.org/docs/18/functions-window.html)
and the [shared ordering of equivalent window specifications](https://www.postgresql.org/docs/18/queries-table-expressions.html#QUERIES-WINDOW).
The datetime oracle is explicitly bounded to these fixtures: fixed-offset times,
whole-year intervals and October 19 timestamps, plus calendar-year date bounds.
It is not a general implementation of PostgreSQL interval arithmetic.

The paired live probe passes **142 checks per engine**: the original 24 queries
over original inputs and two shuffled input orders; 21 frame queries per round
also project row_number over the same sort specification to verify actual
evaluation order. Typed input checks cover all 10,000 tenk rows, employees and
every datetime column. Result type OIDs, values and full witnesses are retained.
The unmodified native PostgreSQL control passes all of these checks.

**113 validation tests pass**. New controls cover all 144 tenk peer orders, an
independent enumerating frame oracle, all six filtered partition representatives,
four opposite-order rank completions, missing/duplicate rows, changed values,
inconsistent peer results, source/header drift, line endings and unrelated errors.
The earlier EXCLUDE TIES bug fix and separate 220-case patched-native oracle remain
unchanged; no bug exception is needed to validate these 24 fixture queries.

CI now explicitly captures raw regression comparisons before running the required
reviewed audit. Capture succeeds only after the complete smoke checks, valid
regression exit status and clean shutdown. Smoke/runner errors, failed isolation,
mixed-workload errors and unrelated failure messages remain failures. The raw
`passed: false` verdict is preserved. The new full execution passes all 12 smoke
checks, reports `capture_complete: true`, and passes the separate 231-case audit.
Automatic PR CI still covers unit/dependency jobs; full-server CI is opt-in.

Runtime source remains `faf62fc4a544b2d033d812b9728b54ecfcf390ad`; postgres SHA-256
is `a5a76016a579d93da8e6cb73c95a419b7130ceba91c140bde6cdb017e2b0717b`.
The preceding validation commit is `fb100a2f5f635d6c95b5ca5301be046f840d4906`.
There is no new runtime or CPU-performance change in this checkpoint.

`engine-*.json` and `probe.json` are compact summaries. `raw-evidence.tar.xz`
contains full probe rows/witnesses, four complete audits, original window
transcripts, all 231 outputs of the new full run, runner reports and owned-cluster
logs. `manifest.json` hashes the archive, summaries, validation sources and inputs.
Prior [MERGE evidence](../merge-contract-2026-10-30/README.md) remains unchanged.

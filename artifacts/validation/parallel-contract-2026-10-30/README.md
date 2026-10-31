# Parallel execution contract: 2026-10-30

The review accepts **229/231** cases on all three retained full runs. Raw
PostgreSQL output remains **203/231**. `merge` and `window` remain unresolved;
every full audit still returns failure.

The new `select_parallel` profile covers 47 direct plans, one source-bound
sort-statistics helper and one PostgreSQL backend-worker counter observation.
Measured root rows/loops remain exact. The fork's reused sort explicitly reports
`Actual Counted: false`; it does not invent an execution count. The worker
statistics remain zero when no PostgreSQL backend workers are launched.

Two source/occurrence-bound error witnesses allow only the absent final worker
stack entry. Complete primary messages and SQL-function stack entries stay
byte-exact. The first serial division-by-zero occurrence remains untouched.
PostgreSQL's [ErrorResponse fields](https://www.postgresql.org/docs/18/protocol-error-fields.html)
distinguish SQLSTATE and primary message from execution context; the live probe
checks all three, including the truthful worker-context difference.

`parallel_contract_probe.py` passes **23 checks per engine** on Linux x86-64:

- Typed fixture values and all 30,000 rescanned-sort results, preserving duplicates.
- Real root rows/loops, sort/nested-loop paths, native worker launches and fork
  omission of unavailable sort metrics.
- Simple and extended query protocols with debug parallelism off/on; the entire
  20,048-byte invalid-input message and SQLSTATE 22P02 match exactly.
- Division by zero (22012), function SET ROLE behavior and caller-role restoration
  after success and failure.
- Aborted-transaction status (25P02), savepoint recovery and return to idle state.
- Cumulative worker statistics: native counters increased by 34 in this sample;
  the fork's counters remained zero. The probe requires positive native deltas,
  not a fixed worker count, and exact zero fork deltas.

**99 validation tests pass**, including changed/shortened error messages, missing
function context, wrong occurrences, line endings, stale source bindings and
malformed worker-statistics rows. The live probe is wired into optional full-server
CI; the unit tests run on every PR.

The runtime remains source `faf62fc4a544b2d033d812b9728b54ecfcf390ad`, postgres
SHA-256 `a5a76016a579d93da8e6cb73c95a419b7130ceba91c140bde6cdb017e2b0717b`.
The preceding validation commit is `c880bc063cfb5dd7092158f57d70236c553ae444`.
This changes validation/profiles/documentation and makes no new CPU or release claim.

`engine-*.json` and `probe.json` are compact summaries. `raw-evidence.tar.xz`
retains the three original transcripts, complete per-case audits, full probe
plans and owned-cluster logs. `manifest.json` hashes inputs, tools, policies,
profiles, summaries and the archive. The preceding
[aggregate evidence](../aggregate-strategy-2026-10-30/README.md) remains unchanged.

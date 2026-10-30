# Diagnostic and SQL choice contract evidence

This checkpoint changes validation and reviewed expectations, not the server
implementation. The runtime remains the binary identified by SHA-256
`a5a76016a579d93da8e6cb73c95a419b7130ceba91c140bde6cdb017e2b0717b`, with runtime
source matching `faf62fc4a544b2d033d812b9728b54ecfcf390ad`. The earlier
[source/binary manifest](../conformance-2026-10-29/README.md) explains its build
label. `012a1e9a7879c3de200ddace74e6f042f58d98da` is the preceding validation commit.

- `engine-fresh.json`: **223/231** accepted, eight unresolved. Correctly reports
  `passed: false`; diagnostic and choice-policy hashes are retained.
- `fresh-suites.json`: twelve smoke checks pass; raw PostgreSQL is **203/231** and
  remains a failing profile.
- `diagnostics-probe.json`: ten checks pass on each engine. Eight queries compare
  typed rows, exact counts and measured EXPLAIN root rows/loops, with required
  Memoize/incremental-sort paths. Cursor allocation accounting and memory-view
  access boundaries are also checked. The different allocator inventory is
  explicitly recorded, not fabricated to match PostgreSQL.
- `validation-tests.log`: **76 unit tests pass**, including root-counter mutation,
  malformed JSON/table/profile rejection, all 216 valid DISTINCT ON choices and
  invalid duplicate-group mutations.
- `raw-evidence.tar.xz`: the fresh candidate's raw regression output/logs and all
  per-case engine audits, including the four captured-table choice proofs and
  eight remaining diffs. `manifest.json` records identities and hashes.

Thirteen source-bound profiles replace 61 exact diagnostic tables: 49 direct
EXPLAIN tables, eleven wrapper results and one allocator query. Separate policy
binds five helper definitions and twelve diagnostic call sites; a profile alone
cannot authorize another helper. Measured root rows/loops and the measured
incremental-sort JSON counts are preserved. No errors or general SQL data are
masked.

The four SQL choice witnesses prove the original captured tables for the named
geometry, create_index, tsrf and sqljson queries. They preserve exact values and
duplicates, require the complete sort-key equivalence or representative/array
membership proof, and are disabled if source or original expectation hashes
change. They do not round finite geometric values, sort arbitrary JSON, or
approve other DISTINCT ON queries. The earlier live choice probe remains useful
independent evidence, but is not substituted for checking the actual transcript.

The fresh run was produced after recording the profiles. The earlier two full
runs are also accepted to the same extent. Native reference output remains in
the [conformance archive](../conformance-2026-10-29/README.md); the preceding
candidate run is in the [qualification archive](../qualification-2026-10-30/README.md).

The full CI server job now includes `scripts/validation/diagnostics_probe.py`;
the command accepts `--reference`, `--candidate`, `--work-dir` and `--output` in
the same manner as the other paired probes. Run with `ulimit -s 65520` on Linux.
Both reviewed and raw regression gates still fail while cases remain unresolved.
This is not production qualification or evidence of mixed-workload CPU savings.

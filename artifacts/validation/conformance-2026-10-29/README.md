# SQL conformance checkpoint

Linux x86-64 validation started on 2026-10-29 and completed on 2026-10-30 UTC.
The manifest records the source base, normalized hashes of every changed runtime
source, the deletion of the simulation module, and exact binary identities.
Windows and Linux runtime source contents were compared before packaging.

The final candidate contains the catalog invalidation race fix, corrected EXCLUDE
TIES semantics, truthful diagnostics and overflow-safe partition sizing.

| Evidence | Result |
| --- | --- |
| `final-catalog-churn.json` | Native and candidate each complete 16,000 concurrent DDL operations |
| `final-compatibility.json` | 88 differential checks pass against unmodified PostgreSQL |
| `final-explain.json` | Seven diagnostic/volatile-expression checks pass |
| `final-choices.json` | Four scoped geometric, DISTINCT ON and JSON proofs pass |
| `window-peer-fixed.json` | Patched native control and candidate each pass 220 independent oracle/minimal-example checks |
| `final-recovery.json` | Ten whole-process kills plus role/timeout and logical restore checks pass |
| `final-suites.json` | Twelve smoke checks and 510 reviewed engine isolation cases pass; raw regression still has 28/231 mismatches |
| `native-regression-control.json` | Unmodified native PostgreSQL passes 231/231 raw cases |
| `window-patched-native-control-final.json` | Separately patched native PostgreSQL passes 231/231, including its added window examples |
| `comparator-tests.log` | 27 comparator tests pass |
| Unit logs in archive | 637 lowering tests, one focused top-sort integration test and 20 invalidation tests pass |

Several JSON reports intentionally say `passed: false`: the pre-fix race and
window reproductions, the old simulated-metrics check, the strict raw regression
profile, and `window-native-divergence.json`. The latter records the approved SQL
bug difference: unmodified native PostgreSQL passes 210/220 window checks while
the corrected candidate passes all 220. These failures have not been masked.

`raw-evidence.tar.xz` retains native and final candidate regression output, server
and isolation logs, complete per-query ordering proofs/residual diffs, and unit
logs. `ordering-summary.json` is only an ordering audit: it leaves plans, errors,
values and unresolved order expressions strict. Three whole cases become equal
under the permitted ordering contract; the four choice probes have narrower,
explicit query scopes. No blanket approval of all remaining cases is implied.

`truthful-explain-cpu.json` measures the diagnostics candidate before the later
catalog/window fixes: four ABBA runs of 5,000 warm EXPLAIN calls. Server CPU was
1.32/1.34 seconds before versus 1.37/1.36 after (about 2.6% higher). It does not
demonstrate a CPU saving or qualify a mixed production workload. The final
candidate is not claimed to have been benchmarked by that report.

The patched native reference is a separate installation. Its upstream patch,
authorship, pending status and hash are documented in
`patches/postgresql/README.md`; vendor files were not edited. This remains a
development checkpoint, not production qualification.

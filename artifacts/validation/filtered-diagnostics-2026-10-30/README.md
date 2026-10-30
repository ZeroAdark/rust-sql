# Filtered diagnostics and partition-read qualification

The reviewed audit accepts **227/231** cases on three independently initialized
full-run outputs. The remaining cases are aggregates, merge, select_parallel and
window. Every full audit still fails overall. Raw PostgreSQL remains **203/231**.

Three added profiles contain 54 exact diagnostic replacements: four EXPLAIN
wrapper results; seventeen partition-pruning plans/wrappers; and fifteen hash
plans plus eighteen projections of unavailable hash counters. The closed policy
binds the original helper definitions, source and exact SQL. Existing upstream
placeholders remain formatting only. Unavailable scalar metrics must be one row
of SQL NULLs; actual data, errors and unmasked root counts remain strict.

Validation against native PostgreSQL 18.6 and the unchanged fork binary:

- **22 checks per engine** compare typed data and real row/loop counts. They cover
  all 10,000 rows/16 columns of the original tenk fixture, the three window cases,
  populated two-level partitions, prepared parameters, nested joins/appends,
  correlated EXISTS, empty MergeAppend results and UPDATE partition routing.
- **A corruption control per engine** changes only owned, stopped cluster files.
  Eight excluded heap pages are corrupted. A generic prepared query still reads
  two healthy rows; queries selecting corrupted partitions at either level fail
  with XX001. The healthy query remains usable. Every file is restored byte for
  byte while stopped, and a restart recovers all eleven original rows.
- **44 hash checks per engine** replay the original 255-statement transaction:
  26 typed data results match pinned fixture values, and 18 metric calls expose
  absent fields as NULL while their actual query values match native PostgreSQL.
  The cases include skew, underestimated sizes, wide values, rescans, full joins,
  low work_mem and parallel settings. No PostgreSQL batch counters are invented.
- **85 validation unit tests** pass, including changed/missing counters, malformed
  JSON, unknown placeholder formats and non-NULL/unexpected scalar metric rows.

`engine-first.json`, `engine-repeat.json` and `engine-fresh.json` retain verdicts
and input hashes. `filtered-probe.json` and `hash-probe.json` provide compact check
summaries and binary identities. Their complete reports, per-case audits, three
cases' raw transcripts from each run and owned probe logs are in
`raw-evidence.tar.xz`. `manifest.json` binds the archive, summaries, validation
sources and fixture inputs. Complete fresh raw suite output also remains in the
[semantic-contract checkpoint](../semantic-contract-2026-10-30/README.md).

Run `filtered_diagnostics_probe.py` and `hash_diagnostics_probe.py` from
`scripts/validation/` as a non-root Linux user, each with `--reference <native-bin>
--candidate <fork-bin> --work-dir <owned-directory> --output <report.json>` and the
documented stack limit. The corruption test never connects to an existing server.
Replay the full audit with the pinned fixtures and corresponding full-run reports.

Runtime source remains `faf62fc4a544b2d033d812b9728b54ecfcf390ad`; the preceding
validation commit is `553828c6530f5f43f2fcba84b3c02e25ee4ec115`. This checkpoint adds
no server work. It does not prove CPU savings, a complete I/O-failure campaign,
or production readiness.

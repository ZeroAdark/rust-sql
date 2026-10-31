# Grouping-set contract evidence

The reviewed audit accepts **224/231** cases on three independently initialized
full-run outputs. Seven cases remain unresolved: aggregates, join_hash, merge,
select_parallel, window, partition_prune and explain. Each full audit still exits
with failure. Raw PostgreSQL comparison remains **203/231**.

The additional profile contains eight exact grouping-set plan tables. Its one
result-table witness computes `coalesce(a,0)+coalesce(b,0)` from the projected
int4 columns, verifies unchanged duplicate-preserving rows and permits movement
only within equal-key groups. Overflow, rounded input, altered aggregate values,
missing rows and changes outside the pinned table are rejected. Profile authoring
uses the separately registered witness; recording a plan cannot authorize a new
result exception.

The independent Python grouping oracle passes ten cases against native PostgreSQL
18.6 and each of the fork's default, disabled and forced rewrite modes: **40
executions**. It checks NULL keys, empty input, duplicate grouping sets, HAVING,
windows over aggregates, target set-returning functions and constant projections.
Typed results and real root row/loop counts are checked. Disabled rewrites use the
original plan family; forced rewrites use Append or Merge Append. The two explicit
UNION ALL controls show that native PostgreSQL also labels the sorted output slot
with its first arm's expression, even when that expression is a constant. Actual
sorting still uses each row's value; both engines return the independently known
ordered results.

Files:

- `engine-first.json`, `engine-repeat.json`, `engine-fresh.json`: audit verdicts
  and input hashes for the three retained runs; full case reports are archived.
- `grouping-probe.json`: check summaries, value hashes, plan families and binary
  identities; complete values and plans are in `probe/report.json` in the archive.
- `validation-tests.log`: **79 passing** comparator, profile, policy and witness tests.
- `raw-evidence.tar.xz`: the three grouping-set transcripts, complete audit
  outputs and owned probe server/initdb logs. No database directories are included.
- `manifest.json`: binary, source, report and archive SHA-256 digests. The complete
  fresh raw regression output remains in the preceding
  [semantic-contract checkpoint](../semantic-contract-2026-10-30/README.md).

Replay the audit with `scripts/validation/engine_regression_audit.py`, the pinned
vendor fixtures and native/candidate full-run reports. Run the live probe with
`scripts/validation/grouping_contract.py --reference <native-bin> --candidate
<fork-bin> --work-dir <owned-directory> --output <report.json>` as a non-root Linux
user with the documented stack limit. Use matching binaries and share files.

This checkpoint changes validation only. The runtime remains the source recorded
at `faf62fc4a544b2d033d812b9728b54ecfcf390ad`; the preceding validation commit is
`d7c5b8fdd98545c36ea6fc0564aef0a594be6709`. It demonstrates no additional CPU or
memory saving and does not establish production readiness.

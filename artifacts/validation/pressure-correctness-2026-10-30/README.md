# Connection churn and physical spill correctness

Runtime: `b2c98c8576d9f1b73e12124c749e5129fcb16c27`; candidate PostgreSQL binary SHA-256 `5c1049ca3dd5d17fb91c7cca088808c4afce8236f62fa1807bcd094d7696555a`. The test changes in this checkpoint do not change the server. Linux x86-64 under WSL Ubuntu 24.04; server affinity 0,2,4,6, clients 8,10. Private delegated cgroup v2 units enforce separate server memory/CPU/PID limits and disable swap. Durability remains enabled.

**Not production qualified.** These are bounded correctness/pressure probes, not performance comparisons or long-duration soaks. The 1 GiB million-row case has a retained watchdog failure and remains an open operating-limit issue. Do not convert its later successful runs into an unconditional pass.

## Connection churn

Each run holds 500 admitted and 300 queued startups per wave while three ordinary-role sessions perform two transactional account updates and one analytical checksum workload. An admin connection remains available. All 800 startups must finish, the extra connection must receive `53300`, queued sessions must drain without timeout/hangup, and every SQL worker must make progress within each five-wave window. Final committed balances and all 97 analytical groups are checked after restart; both normal shutdown exit codes must be zero.

| Cap | Waves / accepted startups | Committed transactions | Analytical queries | Peak cgroup MiB | Settled cgroup MiB |
| --- | --- | --- | --- | --- | --- |
| 4 GiB | 100 / 80,000 | 20,872 | 27,827 | 2,009.145 | 275.559 |
| 2 GiB | 100 / 80,000 | 20,184 | 26,516 | 1,904.332 | 275.035 |

Elapsed run times including settling/restart were 68.46 and 65.75 seconds. Neither run recorded memory-limit, OOM, swap or CPU-throttle events. These do not establish active-query saturation capacity. The script retains per-wave data and checkpoints the complete report; it is intended for short campaigns. A long soak needs streaming reports to avoid increasing checkpoint cost.

## Physical spills

The final 500,000-row run compares six ordered result hashes per server against a bounded-memory Python oracle: wide sorts, aggregation and top-32 selection, with worker limits zero/two and 64 KiB/16 MiB work memory. Actual temporary-file logs witness spilling; low work memory alone is not accepted as proof. Worker limits are configuration values, not a claim that every plan launched workers. Aggregation statements also order their output; their spill witness alone does not isolate each operator's contribution.

Both servers return `53400` for a zero temporary-file limit and recover on the same connection. Cancellation waits for a nonempty live spill file before sending the cancel request; both worker configurations return `57014`, remove temporary files, and execute the exact top-32 query afterward. The cleanup scanner includes shared fileset children, whose filenames do not begin with `pgsql_tmp`. Top-32 queries produced no temporary-file log entries in these runs. Both normal shutdowns exit zero. The setup/query connections are administrative roles; ordinary-role concurrency is covered separately by the churn probe.

At one million rows, the first candidate run completed the serial sort but the watchdog canceled the following parallel-enabled 16 MiB sort at its existing 90% threshold. The server shut down normally without an OOM kill. Native PostgreSQL completed. Four later diagnostic runs on the same server binary completed all result, limit and cancellation checks, but each reached the full 1 GiB cgroup cap. Diagnostic instrumentation additionally captures case-boundary resource snapshots and attempts same-connection recovery if a case fails; therefore the four repeats are not byte-identical harness repetitions of the first failure. This is an unresolved pressure/reclamation boundary, not an established result error or a reason to disable the watchdog.

The final 500,000-row run overlapped part of the third million-row repeat in separate cgroups. No timing/CPU comparison is accepted from any of these runs. Storage also differs: the fork uses `pgrcolumnar`, native PostgreSQL uses heap. `spill-pilot` predates the complete nested-file cleanup check; `spill-cleanup-cancel` predates the extra failure diagnostics. Both are retained as superseded evidence.

## Replay and reproduction

Run `python3 artifacts/validation/pressure-correctness-2026-10-30/replay.py` from any checkout (standard library only). It verifies the compressed archive, all 178 member hashes, harness source hashes, binary identities, resource bounds, startup counters, committed balances, result digests, physical spill/cancellation witnesses and the retained failure. Twelve negative controls reject corrupted verdict inputs; see `checks.json`.

`evidence.tar.gz` contains each exact command, frozen harness sources, reports, controller logs, journals, and private-cluster server/init logs/configuration. Database files are not bundled; sources recreate deterministic data. No executable binary is included. Archive hashing verifies integrity, not independent re-execution or provenance of the original machine.

For a new live run, install Python 3 with psycopg, build the candidate and pinned PostgreSQL reference, and run the harness as a non-root account inside a uniquely named `rustsql-qualification-*.service` with `Delegate=cpu memory pids`. The archived `command.json` records complete `systemd-run` commands; replace paths/account and choose disjoint available server/client CPUs. Parent limits must cover the server plus the separate 2 GiB client allowance. Pass a new output path. Use `spill_probe.py --rows 500000 --memory-mib 1024` or `connection_churn_probe.py --waves 100 --memory-mib 2048`; required candidate/reference/work-dir/output arguments are in each harness's `--help`. The scripts operate only on newly created private clusters.

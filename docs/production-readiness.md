# Production qualification

Status: NOT QUALIFIED. Qualification applies to an exact commit, binary, configuration, architecture and workload. These are acceptance criteria, not a declaration that the gates have passed.

Priority: reduce CPU cost first, while preserving correctness and latency. Memory is a secondary optimization target with explicit operating limits. Report increased metadata, cache capacity and peak RSS alongside CPU savings; do not reject an effective CPU optimization solely because its bounded metadata grows.

| Gate | Required evidence | Current scope |
| --- | --- | --- |
| Build and dependencies | Reproducible locked Linux release build, vulnerability/license review, SBOM | Locked release build and clean RustSec audit; license review/SBOM and independent reproducibility pending |
| SQL compatibility | PostgreSQL 18.6 regression and isolation suites, differential driver/application corpus | The latest allocator binary passes 231/231 reviewed cases (28 raw PostgreSQL mismatches), 510 isolation cases, 87 differential checks, 12 smoke checks, 12 idle-session checks and the independent window oracle. Broader application/driver coverage remains open |
| Durability | Repeated crashes during WAL, checkpoints, columnar compaction and DDL; committed-write verification | Ten whole-process kills preserve committed heap/columnar rows and reject uncommitted writes/DDL; excluded-partition corruption and exact restoration pass; compaction and broader I/O-fault campaigns pending |
| Recovery operations | Restore verified backups; PITR; replica catch-up, failover and upgrade rehearsals | Native PG 18.6 logical dump/restore preserves data, access methods and grants; remaining operations pending |
| Memory safety | Sanitizers, Miri where supported, concurrency models, allocator/JIT review and fuzzing | Upstream facilities exist; full fork campaigns pending |
| Resource limits | cgroup limits at 1/2/4/8 GiB, competing OLTP/OLAP, bounded admission, cancellation, no OOM deaths | The latest allocator binary passes forty mixed trials at 1/2/4/8 GiB, 52 admission checks, 12 connection-contract checks, five connection pairs at 4 GiB and three two-wave startup pilots at 2 GiB. Successful trials have no OOM, swap or memory-limit events. Baseline 2 GiB OOM controls remain retained. Two additional 100-wave mixed connection runs pass at 2/4 GiB. Physical spill/result/limit/cancellation checks pass at 500,000 rows under 1 GiB; a million-row run triggered the memory watchdog before four subsequent diagnostic passes at the cap. This operating boundary, active-query capacity and sustained maintenance remain open |
| Performance | Interleaved baseline/candidate runs; CPU seconds per operation, peak RSS/PSS/cgroup memory, throughput, p50/p95/p99 | Earlier validity forwarding lowered CPU by 4.42-6.08% in its scoped comparison. The latest allocator matrix meets throughput/p99 guardrails and the separate connection calibration reduces peak memory. CPU distributions and limits are reported without a new general CPU-speedup claim in [performance evidence](performance.md#idle-allocator-reuse-2026-10-30). Representative Linux-server qualification remains pending |
| Service security | Authentication/TLS, role boundaries, extension surface and network exposure tests | 21 TLS/SCRAM/certificate/reload/permission checks pass with psycopg 3.1.17 and libpq 16.0.15; broader security and client coverage pending |
| Operational maturity | 24-72 hour mixed-load soak, monitoring, alert thresholds, rollback drill, independent review | Pending |

The preceding transaction-admission binary passes 20 checked mixed-workload trials at 4 GiB. A six-transaction cap keeps CPU cost stable and query p99 changes within 3.07%; a four-transaction cap increases point-read p99 by 14.94% and fails the latency gate. Both results are retained in [the performance report](performance.md#transaction-admission-2026-10-30). These are historical comparisons. The latest allocator binary has its own complete memory matrix below.

The [allocator calibration](performance.md#allocator-selection-2026-10-30) keeps the current Hearth default: mimalloc halves sampled peak cgroup memory but increases median CPU per bundle by 4.20%, reduces throughput by 10.66%, and increases aggregate p99 by 12.16%. All ten trials pass correctness and restart checks; both performance gates fail. Its separate 2 GiB connection pilot passes, but does not justify a CPU/latency regression. The targeted pool work below keeps Hearth.

The subsequent [small-run page-policy experiment](performance.md#sparse-small-run-policy-2026-10-30) is also rejected. Mixed-workload CPU is effectively flat, but controlled 4 GiB connection waves cost 22.23% more CPU per accepted startup by the medians, with an increase in every pair. Three candidate 2 GiB pilots pass with only 18.91 MiB minimum headroom; a fresh baseline still suffers an OOM group kill. That experiment did not repair the retained runtime's 2 GiB connection gate. Only a frame-test measurement correction and its CI check are retained; the retained source passes 112 allocator tests, with one helper ignored by design.

The [idle allocator change](idle-allocator.md) now passes a fresh forty-trial matrix, with all throughput/p99 changes inside 5%, and reduces median 4 GiB connection-burst peak memory by 21.59%. Three two-wave 2 GiB pilots pass with 183.62 MiB minimum headroom. CPU is reported with paired distributions and shutdown cleanup; this is not a general CPU-speedup or production-capacity claim. Fresh SQL, isolation, idle-session, TLS, window-oracle and recovery checks bind to the final binary. Baseline OOM failures and rejected/superseded candidates remain in the [evidence archive](../artifacts/validation/allocator-idle-reuse-2026-10-30/README.md).

The [connection churn and physical spill evidence](../artifacts/validation/pressure-correctness-2026-10-30/README.md) retains 160,000 accepted startups, exact committed-data/restart checks, real spill witnesses and cleanup after limits/cancellation. The million-row 1 GiB watchdog failure remains a qualification blocker even though subsequent runs pass. No server change or new CPU-speedup claim accompanies these probes.

## Acceptance policy

SQL results and protocol remain the compatibility target. Truthful engine-specific EXPLAIN/statistics are accepted; unavailable counters must be absent. See the [approved contract, race reproduction and independent window oracle](compatibility-investigation.md). Raw PostgreSQL output and reviewed engine expectations remain separate profiles.

1. No known result, visibility, isolation or acknowledged-durability regressions. Failures block release even if throughput improves.
2. Compare identical hardware, dataset, query mix, concurrency, build profile and durability settings. Use at least five interleaved repetitions and report distributions, errors and timeouts. Refused or failed queries do not count as useful throughput.
3. Provisional performance gate: no more than 5% regression in throughput or p99 at equal resources; demonstrate a resource improvement on the intended workload. Investigate variance before accepting a threshold crossing. This is a project target, not a measured result.
4. Keep `fsync`, synchronous commit and correctness checks enabled. SQL admission must cover whole transactions; queuing a transaction midway while it holds locks can deadlock the admission gate.
5. Test recovery on disposable clusters. Preserve evidence and exact binary hashes. Promote only after every relevant gate is complete and independently reviewed.

## Compatibility evidence

The initial baseline and cache candidate both passed 202/231 raw regression and 121/122 raw isolation cases. Follow-up investigation fixed skipped-plan CTAS instrumentation and verified the `deadlock-parallel` difference against native PostgreSQL with parallel backend workers disabled. The existing upstream isolation overlay plus extended schedule passes 510/510 cases. This is a distinct engine profile, not a claim that the raw PostgreSQL profile passes. Raw regression counts vary with unspecified peer ordering; preserve each run's output and do not equate a lower mismatch count with that many fixed bugs. See [case classification, evidence and limits](compatibility-investigation.md).

The 12 smoke checks pass. The stronger recovery probe kills the owned supervisor and server process group, checks committed data and uncommitted transactional DDL after ten restarts, restores a logical backup into a fresh cluster, and checks ordinary-role permissions. It also passes under a 1 GiB cgroup. This is limited evidence, not a power-loss, media-corruption, overload, compaction-crash or multi-node recovery guarantee.

The 2026-10-30 loopback-only security probe creates a private cluster with rejecting TCP rules installed before the listener opens. It verifies SCRAM storage/channel binding, TLS 1.2/1.3, server identity and client certificates; rejects bad passwords, unknown/NOLOGIN/expired roles, revoked database access, plaintext, wrong CAs/hostnames and mismatched client identities. A malformed certificate reload retains the previous valid context. A successful rotation changes the peer certificate, preserves existing sessions and accepts new SCRAM-bound sessions. Startup rejects a mode-0644 private key and succeeds again after mode-0600 repair. Ephemeral keys/passwords are excluded from committed evidence. These 21 checks cover one client-library pair, not a complete authentication, crypto or network audit; see [reports and reproducible commands](../artifacts/validation/qualification-2026-10-30/README.md).

For the broad suites, build the tracked PostgreSQL reference in a separate directory (requires `bison`, `flex`, `zlib1g-dev` in addition to the standard dependencies), then run:

```sh
ulimit -s 65520
python3 scripts/validation/server_smoke.py --bin-dir target/release \
  --work-dir /tmp/rustsql-checks --output artifacts/suites.json \
  --regression-dir /path/to/reference-build/src/test/regress \
  --isolation-dir /path/to/reference-build/src/test/isolation \
  --reference-bin /path/to/installed-postgresql/bin
```

The harness returns failure if either suite fails and retains raw diffs. Add `--isolation-overlay regress/isolation-overlay` for the documented engine profile; its extended fixtures require `en_US.utf8` (`sudo localedef -i en_US -f UTF-8 en_US.UTF-8`). The harness stages an independent input tree and never edits vendor fixtures. Its isolation database must be named `isolation_regression` because upstream fixtures reference that name explicitly. The reference was configured with `--without-readline`; keep locale, ICU and build features fixed when comparing runs.

The differential and recovery probes require `python3-psycopg`. Both create private disposable clusters; pass a persistent `--work-dir` to retain evidence:

```sh
python3 scripts/validation/compatibility_probe.py --candidate target/release \
  --reference /path/to/installed-postgresql/bin --work-dir /tmp/rustsql-checks \
  --isolation-dir /path/to/reference-build/src/test/isolation --output artifacts/compatibility.json
python3 scripts/validation/recovery_probe.py --candidate target/release \
  --reference-clients /path/to/installed-postgresql/bin --work-dir /tmp/rustsql-checks \
  --cycles 10 --output artifacts/recovery.json
cargo install cargo-audit --version 0.22.2 --locked
cargo audit --deny warnings
```

The [full Linux server CI run for runtime commit b2c98c8](https://github.com/ZeroAdark/rust-sql/actions/runs/41118637914) passed all three jobs. This verifies the runtime preceding the additional pressure probes, not representative-server performance or production qualification. The full-server CI job runs on branch pushes and remains available manually; it includes these probes, preserves the strict raw verdict, and requires the separate reviewed engine audit. Its explicit capture mode permits comparison exit 1 only after complete smoke checks and a normal suite run; runner failures and unrelated errors still fail. The new full capture and reviewed audit pass locally. The automatic dependency job rejects vulnerabilities and warnings. No advisories are suppressed.

## Deployment work still needed

Use least-privilege service accounts, private network binding, authenticated clients, encrypted transport across trust boundaries, tested backups and externally enforced memory/CPU limits. Size active transactions from measured saturation, not maximum connection count. Apply existing `max_active_queries` and connection-queue controls to ordinary users; upstream exempts superusers from transaction admission, so benchmark with ordinary application roles too.

No production service, existing database, firewall rule or external infrastructure is deployed by this patch. Long-term storage changes are separate projects.

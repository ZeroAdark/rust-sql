# Production qualification

Status: NOT QUALIFIED. Qualification applies to an exact commit, binary, configuration, architecture and workload. These are acceptance criteria, not a declaration that the gates have passed.

Priority: reduce CPU cost first, while preserving correctness and latency. Memory is a secondary optimization target with explicit operating limits. Report increased metadata, cache capacity and peak RSS alongside CPU savings; do not reject an effective CPU optimization solely because its bounded metadata grows.

| Gate | Required evidence | Current scope |
| --- | --- | --- |
| Build and dependencies | Reproducible locked Linux release build, vulnerability/license review, SBOM | Locked release build and clean RustSec audit; license review/SBOM and independent reproducibility pending |
| SQL compatibility | PostgreSQL 18.6 regression and isolation suites, differential driver/application corpus | 88 differential checks and 510 engine-profile isolation cases pass; reviewed regression accepts 227/231, with four unresolved cases; raw remains 203/231; ten additional diagnostic/data/permission checks pass per engine; ten grouping cases pass against native and three fork rewrite modes; 22 additional diagnostic checks and 44 hash-transaction checks pass per engine |
| Durability | Repeated crashes during WAL, checkpoints, columnar compaction and DDL; committed-write verification | Ten whole-process kills preserve committed heap/columnar rows and reject uncommitted writes/DDL; excluded-partition corruption and exact restoration pass; compaction and broader I/O-fault campaigns pending |
| Recovery operations | Restore verified backups; PITR; replica catch-up, failover and upgrade rehearsals | Native PG 18.6 logical dump/restore preserves data, access methods and grants; remaining operations pending |
| Memory safety | Sanitizers, Miri where supported, concurrency models, allocator/JIT review and fuzzing | Upstream facilities exist; full fork campaigns pending |
| Resource limits | cgroup limits at 1/2/4/8 GiB, competing OLTP/OLAP, bounded admission, cancellation, no OOM deaths | Small crash/restore probe passes under 1 GiB, no swap, four-CPU quota, zero OOM events; overload matrix pending |
| Performance | Interleaved baseline/candidate runs; CPU seconds per operation, peak RSS/PSS/cgroup memory, throughput, p50/p95/p99 | Cache benchmarks and small mixed probe; representative workloads and server qualification pending |
| Service security | Authentication/TLS, role boundaries, extension surface and network exposure tests | 21 TLS/SCRAM/certificate/reload/permission checks pass with psycopg 3.1.17 and libpq 16.0.15; broader security and client coverage pending |
| Operational maturity | 24-72 hour mixed-load soak, monitoring, alert thresholds, rollback drill, independent review | Pending |

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

The manual full-server CI job includes these probes and the strict raw suite; it intentionally fails while raw expectations differ. The automatic dependency job rejects vulnerabilities and warnings. No advisories are suppressed.

## Deployment work still needed

Use least-privilege service accounts, private network binding, authenticated clients, encrypted transport across trust boundaries, tested backups and externally enforced memory/CPU limits. Size active transactions from measured saturation, not maximum connection count. Apply existing `max_active_queries` and connection-queue controls to ordinary users; upstream exempts superusers from transaction admission, so benchmark with ordinary application roles too.

No production service, existing database, firewall rule or external infrastructure is deployed by this patch. Long-term storage changes are separate projects.

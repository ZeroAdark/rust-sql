# Production qualification

Status: NOT QUALIFIED. Qualification applies to an exact commit, binary, configuration, architecture and workload. These are acceptance criteria, not a declaration that the gates have passed.

Priority: reduce CPU cost first, while preserving correctness and latency. Memory is a secondary optimization target with explicit operating limits. Report increased metadata, cache capacity and peak RSS alongside CPU savings; do not reject an effective CPU optimization solely because its bounded metadata grows.

| Gate | Required evidence | Current scope |
| --- | --- | --- |
| Build and dependencies | Reproducible locked Linux release build, vulnerability/license review, SBOM | Locked build and unit CI added; audit/SBOM pending |
| SQL compatibility | PostgreSQL 18.6 regression and isolation suites, differential driver/application corpus | 183 targeted tests passed; both binaries have the same 29/231 regression and 1/122 isolation mismatches; classification pending |
| Durability | Repeated crashes during WAL, checkpoints, columnar compaction and DDL; committed-write verification | Small kill/restart smoke harness added |
| Recovery operations | Restore verified backups; PITR; replica catch-up, failover and upgrade rehearsals | Pending |
| Memory safety | Sanitizers, Miri where supported, concurrency models, allocator/JIT review and fuzzing | Upstream facilities exist; full fork campaigns pending |
| Resource limits | cgroup limits at 1/2/4/8 GiB, competing OLTP/OLAP, bounded admission, cancellation, no OOM deaths | Pending |
| Performance | Interleaved baseline/candidate runs; CPU seconds per operation, peak RSS/PSS/cgroup memory, throughput, p50/p95/p99 | Cache benchmarks and small mixed probe; representative workloads and server qualification pending |
| Service security | Authentication/TLS, role boundaries, extension surface and network exposure tests | Pending |
| Operational maturity | 24-72 hour mixed-load soak, monitoring, alert thresholds, rollback drill, independent review | Pending |

## Acceptance policy

1. No known result, visibility, isolation or acknowledged-durability regressions. Failures block release even if throughput improves.
2. Compare identical hardware, dataset, query mix, concurrency, build profile and durability settings. Use at least five interleaved repetitions and report distributions, errors and timeouts. Refused or failed queries do not count as useful throughput.
3. Provisional performance gate: no more than 5% regression in throughput or p99 at equal resources; demonstrate a resource improvement on the intended workload. Investigate variance before accepting a threshold crossing. This is a project target, not a measured result.
4. Keep `fsync`, synchronous commit and correctness checks enabled. SQL admission must cover whole transactions; queuing a transaction midway while it holds locks can deadlock the admission gate.
5. Test recovery on disposable clusters. Preserve evidence and exact binary hashes. Promote only after every relevant gate is complete and independently reviewed.

## Initial compatibility evidence

The candidate and unchanged upstream were each tested with PostgreSQL 18.6's `parallel_schedule` and `isolation_schedule`. Both pass 202/231 regression cases and 121/122 isolation cases in this harness. The failing regression names match. Examined diffs include ordering and EXPLAIN shape/text differences; these are raw expectation mismatches, not 29 proven wrong-result bugs. The isolation mismatch is the ordering of completion output in `deadlock-parallel`. No failures were normalized away or accepted as harmless automatically. Classify each mismatch and test application semantics before calling the engine compatible.

The 12 smoke checks pass, including transactional/columnar updates, rollback, four concurrent writers, clean restart, and one acknowledged write surviving a forced kill. This is limited evidence, not a power-loss, media-corruption or multi-node recovery guarantee.

For the broad suites, build the tracked PostgreSQL reference in a separate directory (requires `bison`, `flex`, `zlib1g-dev` in addition to the standard dependencies), then run:

```sh
ulimit -s 65520
python3 scripts/validation/server_smoke.py --bin-dir target/release \
  --work-dir /tmp/rustsql-checks --output artifacts/suites.json \
  --regression-dir /path/to/reference-build/src/test/regress \
  --isolation-dir /path/to/reference-build/src/test/isolation \
  --reference-bin /path/to/installed-postgresql/bin
```

The harness returns failure if either suite fails and retains raw diffs. Its isolation database must be named `isolation_regression` because upstream fixtures reference that name explicitly. The reference was configured with `--without-readline`; keep locale, ICU and build features fixed when comparing runs.

## Deployment work still needed

Use least-privilege service accounts, private network binding, authenticated clients, encrypted transport across trust boundaries, tested backups and externally enforced memory/CPU limits. Size active transactions from measured saturation, not maximum connection count. Apply existing `max_active_queries` and connection-queue controls to ordinary users; upstream exempts superusers from transaction admission, so benchmark with ordinary application roles too.

No production service, existing database, firewall rule or external infrastructure is deployed by this patch. Long-term storage changes are separate projects.

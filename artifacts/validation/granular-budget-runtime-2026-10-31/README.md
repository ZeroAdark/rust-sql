# Granular-budget runtime qualification

Exact candidate `635ec740b3cd85c4318eadc87843705032494a49d92d1deeb79b09a8c5ab9e57` passes the established local runtime checks. It is locally retained after the separate [pressure/performance](../granular-budget-reclaim-2026-10-31/README.md) and [resource](../granular-budget-resources-2026-10-31/README.md) gates. Production qualification and PostgreSQL memory/transactional-tail parity remain open. GitHub Actions remain disabled.

The source is parent `b3d1a667` plus the seven allocator/budget source and test files recorded in the build report. The locked release build reports 181 passing tests, one ignored test and three negative controls that fail at the intended assertions. The archive preserves the original source-file ownership setup failure separately; no tests or build started in that failed setup.

| Check | Result |
|---|---|
| Reviewed SQL compatibility | 231/231 accepted, no unresolved cases |
| Raw PostgreSQL expectations | 203 pass; 28 documented differences |
| Engine isolation profile | 510/510 pass |
| Smoke / idle-session / connection-contract checks | 12 / 12 / 12 pass |
| Admission / differential SQL checks | 52 / 87 pass |
| Authentication and TLS checks | 21 pass |
| Recovery | Ten whole-process kill/restart cycles, logical restore and ordinary-role checks pass |
| Independent window oracle | 220 checks per engine pass against separately corrected PostgreSQL |
| Bind and prepared-statement lifecycle | Eight groups per engine agree with native PostgreSQL; 192 committed updates per engine |

The raw PostgreSQL verdict remains a failure and is preserved separately from the approved engine compatibility profile. Truthful EXPLAIN/statistics differences and permitted unspecified ordering are not described as repaired SQL-result bugs. The existing native regression control is reused with its exact reports, transcripts and hashes. The window oracle uses the separately patched PostgreSQL reference for the documented EXCLUDE TIES difference.

The extra Bind check covers three plan-cache modes, large/null/by-reference parameters, arrays and numeric values, malformed Bind cleanup before and after parameter registration, execution errors and rollback, holdable cursors across commit, and plan revalidation after DDL. It requires exact engine agreement and clean shutdown. Functional suite runs use CPU quotas and are not performance evidence.

Run `python3 replay.py` with assertions enabled. The verifier checks all 3,176 archived members, exact source/binary identities, build controls, suite counts, SQL transcript hashes, all 231 case proofs, isolation results, protocol outcomes and security/recovery reports. An altered SQL transcript is rejected during archive creation. Replay passes on Windows and Linux; it reads evidence without extracting or executing archive contents and does not rerun SQL.

This checkpoint closes the reproduced allocator pressure failure for the tested workloads and limits. It does not establish arbitrary-query OOM immunity, power-loss/media-failure durability, maintenance/compaction safety, PITR/failover/upgrade readiness, broader client compatibility, sanitizer/fuzz coverage or a 24–72 hour soak. These production gates remain open.

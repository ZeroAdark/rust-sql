# Transaction admission and cancellation

The fork's `max_active_queries` limit applies to ordinary users at transaction boundaries. A transaction keeps its slot until completion, including while idle or awaiting rollback after an error. This prevents lock holders from queuing behind transactions that are waiting for their locks. Superusers, replication, session advisory-lock owners and prepared-transaction finishers retain the documented exemptions. A limit of zero disables admission.

## Defects and fixes

The preceding fork allowed extended-protocol queries to bypass the limit. Parse and Bind started a transaction before Execute reached admission. Execute correctly avoided parking a transaction that might already hold locks, but consequently neither parameterized queries nor cached prepared statements were counted. The fix admits Parse, Bind and both Describe paths before their first transaction starts. Fast-path function calls now also acquire admission. Execute retains its check for a portal used in a new transaction. Slots survive across protocol messages and release at transaction end or backend exit.

Two additional defects affected queued work:

- A shorter configured `transaction_timeout` suppressed `statement_timeout` even before a transaction existed. Only an **active** transaction timer can now suppress that statement timer, so the queue wait remains bounded by a configured statement timeout.
- An infinite condition-variable sleep periodically checked its wake list internally but did not return to admission to read a raised or disabled limit. Admission now requests an explicit one-second timed wait. Configuration changes become visible at the next recheck without requiring another transaction to finish first. Normal releases still wake one waiter immediately.

The admission fast path and individual counter/wake operations use constant-size state and expected O(1) work. Each parked waiter performs a bounded recheck once per second; aggregate recheck work is linear in the number of waiters, bounded by the connection limit. No query queue scan or per-row metadata was added. At saturation, recognizing a prepared-transaction finisher from new SQL retains the existing O(SQL length) raw-syntax check; cached statements use their stored command tag. SQL parsing is never added to an uncongested admission.

## Validation

[`admission_probe.py`](../scripts/validation/admission_probe.py) creates a private Unix-socket cluster and sends actual protocol 3.0 frames as an ordinary role. It checks all seven entry paths with statement timeout and cancellation, verifies queued clients hold no lock on the test relation, and checks that sessions remain usable afterwards. Further cases cover:

- A 16-client burst, eight CancelRequests over separate sockets, and complete draining of the remaining eight clients.
- Parse/Flush holding a slot until Sync, pipeline error recovery, explicit aborted transactions, EOF and administrative termination.
- A row-lock holder continuing through extended execution while another transaction queues.
- COMMIT/ROLLBACK PREPARED through Parse, cached Bind and Describe, plus session advisory-lock release.
- A configured but unarmed transaction timeout, and raising, lowering and disabling the admission limit by configuration reload.

The candidate passes all **52 checks**, with final active, waiting and connection-queue counts all zero. The same harness reports 21 failed checks against the preceding binary and then times out during the disable/reload case; that incomplete negative control is preserved as a failure, not a completed suite. Separate investigation of the intermediate build confirmed the timer and reload failures after the protocol entry points had been fixed. Ten fresh whole-process crash/recovery cycles and logical dump/restore checks also pass.

The same binary passes 12 smoke checks, 87 PostgreSQL differential checks, all 510 isolation cases and a fresh 231/231 reviewed regression audit. The raw PostgreSQL comparison remains 203/231 and its failing verdict is retained. All 79 PostgreSQL session unit tests and 126 Python validation tests pass.

Two five-pair mixed-workload calibrations retain stable CPU cost. Enforcing four active transactions increases point-read p99 by 14.94% and fails the provisional latency gate. A six-transaction cap passes that gate, with at most a 3.07% increase across query p99 medians. See the [complete performance results and tradeoffs](performance.md#transaction-admission-2026-10-30) and [retained evidence](../artifacts/validation/transaction-admission-2026-10-30/README.md).

```sh
ulimit -s 65520
python3 scripts/validation/admission_probe.py \
  --candidate /path/to/fork/bin --work-dir /tmp/rustsql-checks \
  --output artifacts/admission.json
```

The probe uses a reloadable configuration file: a command-line `-c max_active_queries=...` setting takes precedence over `ALTER SYSTEM` and cannot demonstrate a successful reload. The optional full-server CI job runs this probe. Source and binary hashes, raw reports and retained cluster logs accompany the validation evidence.

These checks validate admission behavior, not a production capacity or memory limit. Connection-queue saturation now has a separate [implementation and probe](connection-queue.md). Smaller-memory connection pressure, large spills, memory-pressure recovery, long-duration overload and representative Linux-server qualification remain separate gates in [production readiness](production-readiness.md).

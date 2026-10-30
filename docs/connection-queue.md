# Bounded connection admission

`connection_queue_size` limits ordinary connections waiting for a backend slot before authentication. Each waiter already owns a socket and a backend thread. Waking a waiter does not guarantee that it has acquired a slot, so it must continue to count against the limit until it leaves the queue.

The preceding implementation removed a waiter from its deque on wake and used the deque length for capacity. A newcomer could consume the apparent vacancy before the woken waiter acquired a slot or rejoined the deque. This admitted more pending connections than the configured bound. Cancellation and requeue also searched the deque linearly.

The fix tracks pending ownership separately from the set of unsignaled waiters. A guard releases that ownership on admission, timeout, disconnect or shutdown. Ordered, nonwrapping tickets retain each waiter's original position when wakeups race; a B-tree supports arbitrary cancellation without scanning other waiters. Capacity reads and updates are O(1); insertion, wake selection, requeue and cancellation are O(log n), with O(n) bounded metadata. The existing 100 ms interrupt/hangup and slot-retry tick remains: aggregate polling is linear in the number of parked clients. Admission is approximately FIFO because retries can race for slots.

The reproducer fails on the preceding implementation: after a wake, the reported count drops from one to zero while the waiter still owns its socket and thread. It passes with the fix. Three additional tests cover original-ticket ordering and cancellation, a 20,000-step lifecycle comparison against an independent model, and ticket exhaustion without wrapping. All 26 `lmgr_proc` unit tests pass, including with the default parallel test runner.

## Protocol and burst validation

[`connection_queue_probe.py`](../scripts/validation/connection_queue_probe.py) creates disposable clusters with private Unix sockets. Its 12 contract checks cover queue capacity and overflow SQLSTATE 53300, timeout, hangup, draining, reserved roles, superuser access, forged bypass claims and shutdown. Both the preceding and fixed binaries pass these contract checks; the deterministic unit reproducer specifically exposes the capacity-accounting race.

For pre-authentication shutdown, PostgreSQL suppresses remote output and a queued client can receive clean EOF. The probe accepts EOF only in that shutdown case. Unexpected EOF during other startups remains a failure. This follows `ProcessInterrupts` in the tracked PostgreSQL 18.6 reference; the older module comment describing an administrator-command error on every shutdown does not describe this authentication-stage behavior.

The burst mode holds 800 ordinary startup connections at once: 500 admitted and 300 queued. One additional attempt must be rejected. It then releases admitted clients, requires every queued client to become ready, and verifies the counters return to zero before repeating the wave. The final fixed-binary run passes both waves: 1,600 accepted startups, two intentional overflow rejections, and no timeouts, hangups or leftover server processes. It checks actual thread CPU affinity while connections are held.

The server has a separate 4 GiB cgroup, no swap, four server CPUs, and a 1,024-task limit. Client work uses separate CPUs and a separate cgroup. The final run peaks at **2,573,369,344 bytes (2.40 GiB)** of server cgroup memory, with no memory-limit, OOM or CPU-throttling events. This short startup/drain probe is not a sustained-load test or a CPU-performance comparison. A subsequent run with the same 500-connected-plus-300-queued configuration hits the 2 GiB limit and is killed as an OOM group. Disabling huge pages specifically for glibc-created thread stacks does not prevent that failure. A fresh 4 GiB control passes at a 2.49 GiB peak; the [passing and failing pressure evidence](../artifacts/validation/connection-pressure-2026-10-30/README.md) retains the kernel counters. This configuration is not qualified at 2 GiB; the substantial anonymous/huge-page footprint remains under investigation. No host-wide huge-page settings or default connection limits were changed.

The same binary passes 52 transaction-admission checks, 87 differential checks, 12 smoke checks, all 510 isolation cases, ten whole-process crash/recovery cycles and logical restoration. The reviewed regression audit accepts 231/231 cases; the raw PostgreSQL comparison remains 203/231 and failed. All 126 Python validator tests pass. See [hashed reports, sources and logs](../artifacts/validation/connection-queue-2026-10-30/README.md).

```sh
ulimit -s 65520
cargo test --locked -p lmgr_proc --lib
python3 scripts/validation/connection_queue_probe.py \
  --candidate /path/to/fork/bin --work-dir /tmp/rustsql-checks \
  --output artifacts/connection-queue.json
```

Burst mode additionally requires a delegated cgroup v2 service running as an ordinary user, with the CPU, memory and pids controllers delegated. Inside that service, choose available disjoint CPU sets and run:

```sh
python3 scripts/validation/connection_queue_probe.py --burst \
  --candidate /path/to/fork/bin --work-dir /tmp/rustsql-checks \
  --output artifacts/connection-burst.json --memory-mib 4096 \
  --server-cpus 0 2 4 6 --client-cpus 8 10
```

The automatic CI unit job includes `lmgr_proc`; the optional full-server job includes the 12 contract checks. The host-specific burst is retained as local WSL development evidence. Full resource matrices for this binary, large spills, pressure recovery, long overload/maintenance runs and representative Linux-server qualification remain open in [production readiness](production-readiness.md).

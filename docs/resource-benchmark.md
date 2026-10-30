# Fixed mixed-work resource benchmark

This benchmark measures server CPU for a constant amount of checked SQL work.
It is one qualification workload, not a general database benchmark or an overload
guarantee. The previous time-limited mixed probe could finish different proportions
of transactional and analytical queries; its CPU/operation result must be read
alongside those proportions.

## Work and correctness

The default dataset has one million columnar fact rows, 128 dimension rows and
120,000 indexed heap accounts. Six ordinary-role clients each perform 50 warmup
bundles and 400 measured bundles. Every bundle contains twelve point reads, four
autocommitted updates, one full grouping aggregate, one 4,096-row selective
aggregate and one dimension-join aggregate: exactly nineteen statements. Each
trial therefore measures 45,600 statements, including 9,600 acknowledged updates.
The complete forty-trial matrix measures 1,824,000 statements.

Every result and wire type is checked. Independent arithmetic over the generator's
bounded period supplies aggregate expectations without scanning the input table
in the client. Clients own disjoint account ranges. The controller independently
recomputes every changed balance, checks the total acknowledged updates including
warmup, then performs the same checks after a clean server restart. This restart
check does not replace crash, power-loss or storage-fault testing.

Prepared statements and warmup favor stable execution. This workload does not
exercise program-cache eviction pressure, high-cardinality hash tables, large
sort spills, update contention, concurrent DDL, complex application queries or
all PostgreSQL drivers. No claim about those workloads follows from these results.

## Accounting and limits

A private, ordinary-user systemd unit delegates cgroup v2 CPU, memory and PID
controllers. Its driver child contains setup, clients, result verification and
sampling; each server trial gets a fresh sibling cgroup. All server descendants
inherit that group. CPU counters retain exited descendants' consumption, avoiding
a fixed PID list that could miss worker processes. The sampler rereads the process
membership for RSS and PSS. Live positive and negative controls verify exited-child
CPU, client exclusion, resident allocation sampling and an intentional OOM in a
separate disposable non-database process.

Server limits are four CPUs, no swap and 1/2/4/8 GiB of charged memory. Clients use
two different logical CPUs and a separate 2 GiB ceiling. Choose disjoint available
CPU sets for the target host. The measurement retains CPU user/system time,
throttling, memory events, sampled memory.current every 50 ms, sampled summed
RSS/PSS every 500 ms, and cgroup memory.peak. The latter includes startup, setup
and restart; sampled peaks cover the measurement window. RSS can double-count
shared pages; PSS apportions them. Cgroup memory is charged ownership, so shared
file pages first charged elsewhere can appear in RSS/PSS without appearing in
that server group's memory.current. The cap is not a promise that RSS stays below
the same number. Report these measures separately.

Server CPU excludes client CPU and is not total machine energy or all asynchronous
kernel work. Client/protocol processing and result verification contribute to
observed bundle latency. All per-statement timings are counted, with exact
nearest-rank p50/p95/p99 summaries; no latency reservoir sampling is used.
Throughput counts completed, verified bundles. Rejected or failed trials cannot
be reported as successful performance runs.

Both binaries use identical durability settings (`fsync=on`,
`synchronous_commit=on`), `max_active_queries=4`, `work_mem=16MB` and disabled
autovacuum for this bounded campaign. The original memory governor remains active;
it can reduce effective concurrency below the configured query maximum. Disabling
autovacuum and using a short generated dataset do not qualify sustained storage
maintenance behavior.

Five repetitions reverse both engine and memory-limit order on alternating rounds.
The comparison preserves every paired observation, median and range, and reports
per-query latency alongside CPU/bundle and throughput. It verifies the complete
matrix, exact workload counts, source hashes, result types, durability settings,
counters and restart state before comparing anything. It reports the provisional
5% throughput/p99 thresholds; statistical uncertainty and release acceptance still
require review. A lower median alone does not prove an improvement.

## Reproduce on Linux

Install Python 3 and psycopg 3, build both release binaries first, and run from an
ordinary-user shell on an otherwise idle host with systemd and cgroup v2. Keep
assertions enabled: do not use `python -O` or `PYTHONOPTIMIZE`. Use a new output
directory for every campaign. Select CPU IDs after inspecting the host's topology.

```sh
sudo systemd-run --unit="rustsql-qualification-mixed-$(date +%s)" \
  --uid="$(id -u)" --gid="$(id -g)" --collect --wait \
  --property='Delegate=cpu memory pids' \
  --property=MemoryMax=12G --property=MemorySwapMax=0 \
  --property=CPUQuota=600% --property=LimitSTACK=67108864 \
  --property=RuntimeMaxSec=3600 \
  /usr/bin/python3 -u /absolute/repo/scripts/validation/resource_workload.py \
  --baseline /absolute/upstream/bin --candidate /absolute/fork/bin \
  --work-dir /absolute/disposable-clusters --output-dir /absolute/new-results \
  --server-cpus 0 2 4 6 --client-cpus 8 10 \
  --memory-mib 1024 2048 4096 8192 \
  --rows 1000000 --clients 6 --bundles 400 --warmup 50 --repeats 5

python3 scripts/validation/compare_resource.py \
  --input-dir /absolute/new-results --output /absolute/new-comparison.json
```

Keep the `--wait` caller attached until the service terminates. In WSL, detached
systemd services alone do not keep the distribution running. An early calibration
was interrupted by a WSL shutdown; its incomplete report is retained and excluded
from comparisons. The harness now leaves `passed=false` until the entire campaign
is complete, even when the completed individual trials passed.

For accounting controls, use another fresh delegated unit with the same ownership
and controller properties, replacing the workload command with
`python3 /absolute/repo/scripts/validation/resource_scope_probe.py --output /absolute/new-control.json`.
That control intentionally kills its own small allocation process under a private
32 MiB limit. It does not run a database or change host-wide limits.

The kernel's [cgroup v2 documentation](https://docs.kernel.org/admin-guide/cgroup-v2.html)
defines these counters and memory charges. systemd's
[resource-control documentation](https://www.freedesktop.org/software/systemd/man/latest/systemd.resource-control.html)
describes delegated controllers. A WSL workstation result remains development
evidence; repeat on the intended Linux server before qualification.

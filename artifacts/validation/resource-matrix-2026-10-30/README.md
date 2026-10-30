# Fixed mixed-work resource matrix

All forty trials passed their workload, result-type, cgroup and clean-restart
checks. The campaign measured **1,824,000 statements**, including **384,000 committed
updates**, across five paired repetitions at each of 1/2/4/8 GiB. Including warmup,
432,000 acknowledged updates were checked again after restart. No database trial
recorded OOM, swap, memory-limit events, CPU throttling or a workload error.

This is not production qualification. Median CPU/bundle changes versus upstream
are **+0.10%, -0.06%, +0.03% and +0.95%**, respectively: no CPU saving is demonstrated.
All median throughput and query-specific p99 changes satisfy the provisional 5%
regression threshold. The full distributions include variation and the 8 GiB CPU
increase; a passing latency threshold does not establish the CPU goal.

The [comparison](comparison.json) retains all paired measurements, median/min/max,
query-specific p50/p95/p99 and RSS/PSS/charged-memory peaks. See the
[method and reproduction commands](../../../docs/resource-benchmark.md) and
[performance interpretation](../../../docs/performance.md).

## Provenance

- Upstream source: `38634b408dea373f3944006dfbf74d19cbfc409f`.
- Upstream binary SHA-256: `bd9c8954d1c58ac3c0dcea5035af7cf44fc7c0e4bf8acb243a034132781d35e3`.
- Fork runtime source: `faf62fc4a544b2d033d812b9728b54ecfcf390ad`.
- Fork binary SHA-256: `a5a76016a579d93da8e6cb73c95a419b7130ceba91c140bde6cdb017e2b0717b`.
- Preceding validation commit: `d14297696345029ab5b4f13ffffedfffd16e0325`.
- Ubuntu 24.04, WSL2 Linux x86-64, Ryzen 7 9800X3D; Python 3.12.3,
  psycopg 3.1.17 and libpq 16.0.15. Server guest CPUs 0/2/4/6; clients 8/10.
- 124 validation unit tests and Python compilation pass. Four live accounting
  controls pass, covering exited-child CPU, client exclusion, resident allocation
  sampling and an intentional OOM of a separate small allocation process.

The runtime has not changed in this checkpoint. Version banners alone do not
identify these development binaries; use their hashes and retained source
provenance. WSL and other host activity limit generalization to a production
server. The workload's stable prepared statements do not exercise cache eviction,
large spills, sustained maintenance or overload.

## Retained evidence

[`evidence.tar.gz`](evidence.tar.gz) contains 148 files: every matrix report,
metadata and completion summary; all forty server/initdb logs; the full comparison;
accounting controls; preliminary and interrupted calibrations; systemd completion
and shutdown journals; the validation-test log; and exact LF-normalized snapshots
of the eight new or changed Python sources.

Archive size: **1,470,161 bytes**. SHA-256:
`c13b6d19782a5756b1f363699a4508d0e0913e1173cb5eef6b9158a1391a9982`.
[`manifest.json`](manifest.json) records every member's hash and source hashes.

The original detached calibration stopped during WSL shutdown after only its
baseline trial. Its old summary has `passed=true` but `complete=false`; it is
explicitly incomplete and excluded. The corrected runner leaves the overall
verdict false until completion. The full matrix used an attached `systemd-run
--wait` command, which returned exit 0 after 15 minutes 46.879 seconds. The
comparison rejects incomplete, missing, duplicate or inconsistent evidence.

To inspect and independently recompute the comparison after extracting into a
new directory:

```sh
python3 extracted/harness/compare_resource.py \
  --input-dir extracted/matrix --source-dir extracted/harness \
  --output recomputed-comparison.json
```

The database directories remain local and are not in this archive. The retained
logs and reports contain generated benchmark data, not application data.

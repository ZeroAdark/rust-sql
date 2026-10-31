# Sustained sorts and rejected prefault ceiling (2026-10-31)

The retained server is `0be60915b40c9f70f39dcb3a0fd1f8918608a20095f767ec081a5b5320caa277`.
Experimental `4d0518fe` is **rejected**: its CPU and latency preservation guards pass, but both charged memory
and process PSS increase at medium load and capacity. Its runtime source changes were reverted.

## Pressure evidence

The archive preserves 33 completed trials, 380 successful full-result sorts and three watchdog cancellations.
Each trial loads one million rows with a 128-byte text payload into a disposable database, then alternates
64 KiB/serial and 16 MiB/parallel sort settings under a 1 GiB server cgroup with swap disabled. Server and driver
use disjoint hard CPU sets on this PC under WSL. COPY output is checked against an independent full-order digest;
physical spill files and cleanup are recorded. All completed trials have clean shutdowns and no kernel OOM kill.
Pressure timings include transport and client digest work and are not qualified performance comparisons.

| Campaign | Trials passing / completed | Successful sorts |
|---|---:|---:|
| Fresh ordinary connection: PostgreSQL, Rust default, Rust 64 MiB maximum | 9 / 9 | 108 |
| Reused load connection: PostgreSQL, Rust with/without observer, Rust observer + 64 MiB | 10 / 12 | 130 |
| Prefault ceiling: PostgreSQL, retained Rust with observer, candidate with/without observer | 11 / 12 | 142 |

The ownership SQL observer changes allocator behavior: two retained-default trials in the second campaign and
one in the third hit watchdog SQLSTATE 53200. Failed trials remain in the archive. Explicit 64 MiB configuration
and the candidate's derived ceiling avoid cancellation in their tested repeats. All Rust arms still reach the
1 GiB lifetime cgroup peak during setup/workload; PostgreSQL peaks near 691 MiB. The pressure gate remains open.

An earlier observer run fails before any query because its Unix socket pathname is too long. Its report, command
and log are retained separately as a harness setup failure. All completed campaigns check acknowledged row
count and ordered top-32 data after restart; only the newest also checks the complete ordered digest after restart.

## Performance decision

Twenty-five separate trials cover 1,761,300 timed statements: five repetitions of each of three engines at medium
load and five pairs of the two Rust builds at longer capacity. Both phases use one million rows, six clients,
2 GiB server limits, synchronous commits, hard CPU sets and correctness/restart checks. Capacity uses 1,500 bundles
per client; medium uses 30, paced to 9.4 bundles/second. Warmup is excluded from timing.

| Candidate relative to retained Rust | Medium | Capacity |
|---|---:|---:|
| CPU per bundle | -0.64% | +0.05% |
| Charged memory | **+3.70%** | **+2.87%** |
| Process PSS | **+3.28%** | **+2.43%** |
| Point-read p99 | -3.14% | +0.73% |
| Commit p99 | +0.67% | +1.01% |
| Range p99 | +2.10% | +4.08% |
| Bundle p99 | +0.31% | +2.36% |
| Throughput | rate limited | -1.04% |

All unchanged 5% CPU/throughput/p99 guards pass. Memory does not improve, so the candidate is rejected.
The screen's `passed` field covers pressure and preservation checks only; `retention-assessment.json` records
the rejection. Low baseline measurements and every trial remain included. Capacity has no PostgreSQL arm and
supports no new capacity speedup claim against PostgreSQL.

The rejected build passes 84 allocator/memory-runtime tests. A deliberate broken-ceiling control fails the new
allocator test, and the restored implementation passes. Intended behavior alone does not establish benefit.

## Replay and limits

Run `python replay.py` with assertions enabled. The standard-library verifier checks archive/member hashes,
source and executable identities, resource boundaries, result digests, spills, errors, recovery, raw performance
records and the rejection decision. It rejects forged row counts, executable identity and CPU affinity. It has
passed on Windows and Linux. This verifies recorded evidence; it does not rerun SQL or qualify production.

`summary.json` provides pressure totals. The archive contains frozen harnesses, results, logs, commands, build
tests and the rejected patch. The newer repository `sustained_sort_probe.py` includes its own CPU-set wrapper;
separate two-cycle PostgreSQL/Rust smoke checks pass, including CPU-set escape rejection. Its source differs from
the archived campaign versions. No GitHub Actions were used.

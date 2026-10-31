# Memory localization and ownership diagnostics

All 78 diagnostic cases pass their SQL and cleanup checks: 12 pilot cases, 48 category cases, six ownership probes and 12 stabilized-startup cases. The retained Rust binary is `02cfbca6`; PostgreSQL is native 18.6. Every category uses a fresh cluster, a restart after loading, six ordinary clients, independent result/type oracles and committed-state checks across a further restart. The main category campaign tests 10,000 and one million fact rows twice; each server has 2 GiB, four physical cores, 128 MiB shared buffers and 16 MiB work_mem. No OOM, memory-limit event, swap use or unclean shutdown occurred.

The table shows median process PSS in MiB at one million rows. Pausing clients at synchronized stages permits memory sampling and background reclamation; these are diagnostic observations, not performance qualification.

| Category / engine | Before clients | Six connected | First query | Repeated queries | Disconnected + 1 s |
| --- | ---: | ---: | ---: | ---: | ---: |
| idle / postgresql | excluded | 34.6 | 34.6 | 34.6 | 22.7 |
| idle / rustsql | 169.2 | 210.5 | 210.5 | 210.5 | 196.4 |
| read / postgresql | excluded | 34.5 | 35.0 | 35.8 | 22.6 |
| read / rustsql | 170.7 | 211.1 | 215.3 | 217.5 | 199.7 |
| update / postgresql | excluded | 34.7 | 35.4 | 36.3 | 22.7 |
| update / rustsql | 167.0 | 212.4 | 215.9 | 219.8 | 204.5 |
| range / postgresql | excluded | 34.5 | 53.6 | 78.9 | 22.7 |
| range / rustsql | 166.8 | 210.2 | 223.2 | 234.7 | 223.6 |
| aggregate / postgresql | excluded | 34.5 | 57.6 | 79.3 | 22.8 |
| aggregate / rustsql | 173.1 | 216.3 | 300.3 | 325.8 | 265.1 |
| join / postgresql | excluded | 34.6 | 56.5 | 79.9 | 22.8 |
| join / rustsql | 182.7 | 227.1 | 303.7 | 285.6 | 264.6 |

The verifier found 27 incomplete PostgreSQL startup PSS observations in the original pilot/category runs: the readiness connection can exit during process enumeration. They remain archived and are excluded from summaries; the table omits the original PostgreSQL startup cells. All connected-workload observations are complete. A separate 12-case campaign waits for two consecutive complete samples with the same process set (20 ms between attempts, at most 50 attempts). It retains every retry decision. Its startup results are:

| Rows | Engine | Stable startup PSS MiB | Repeats |
| ---: | --- | ---: | ---: |
| 10000 | postgresql | 22.8 | 3 |
| 10000 | rustsql | 169.6 | 3 |
| 1000000 | postgresql | 22.8 | 3 |
| 1000000 | rustsql | 184.7 | 3 |

Rust's startup and idle-session overhead exists before analytical queries. Aggregate and join execution add anonymous memory, much of it backed by huge pages; some remains after clients disconnect. The large virtual mapping sizes are not physical allocations. PostgreSQL's process PSS falling after disconnect does not mean its charged filesystem cache disappears.

The separate ownership probe keeps one extra administrative connection, queries the memory diagnostic functions, and samples physical memory before and after them. These observations are not merged into the table. First diagnostic calls add roughly 12-21 MiB; one later join observation adds 32 MiB. This observer effect is retained, not attributed to the workload.

Pool counters point to small-allocation capacity slack and warm/free capacity alongside live allocations. They are allocated capacity, not necessarily resident memory: after aggregate execution the free-capacity counter can exceed total physical PSS. The owner table also lists a 128 MiB shared-buffer reservation while only about 6.4 MiB is physically touched in these Rust probes. Neither counter justifies subtracting that full value from RAM estimates or changing reclamation policy blindly. The evidence narrows future work to fixed/session allocations and execution-buffer residency, with CPU and latency guards required for any change.

The archive includes raw observations, source, commands, logs and per-stage account/owner rows. `python3 replay.py` verifies all 78 cases, exact binaries and source hashes, result/type counts, independent committed-state digests, complete summarized PSS readings and clean resource teardown. The 27 excluded startup observations are checked explicitly. Altered state digests and new incomplete samples are rejected. It does not rerun SQL or execute archived code. No runtime/default is changed by these diagnostics. GitHub Actions remained disabled.

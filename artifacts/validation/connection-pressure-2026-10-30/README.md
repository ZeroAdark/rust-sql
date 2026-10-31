# Connection memory-pressure controls

**The 2 GiB workload fails and remains a qualification blocker.** This follow-up uses the same runtime binary as the [queue fix](../connection-queue-2026-10-30/README.md), with 500 connected and 300 queued ordinary clients. It changes no runtime source or host setting. The probe now captures cgroup counters even when a startup fails or the server is killed.

| Private service configuration | Result | Server peak | Completed waves |
| --- | --- | ---: | ---: |
| 2 GiB, inherited allocator/thread settings | OOM group kill; connection reset | 2 GiB limit | 0 |
| 2 GiB, `GLIBC_TUNABLES=glibc.pthread.stack_hugetlb=0` | OOM group kill; connection reset | 2 GiB limit | 0 |
| 4 GiB, inherited settings | Pass; 1,600 accepted startups and two intentional overflow refusals | 2,674,262,016 bytes (2.49 GiB) | 2 |

The thread-stack experiment follows the [glibc tunable documentation](https://sourceware.org/glibc/manual/latest/html_node/POSIX-Thread-Tunables.html). It did not remedy this workload's memory pressure. The host reports glibc 2.39 and transparent huge pages in `madvise` mode. No production recommendation or default change follows from this experiment, and incomplete failing runs cannot be compared as successful-throughput or CPU measurements.

[checks.json](checks.json) preserves the failing verdicts, OOM counters, limits, exceptions and service invocation IDs. All three server cgroups are empty after cleanup. The 4 GiB control has no memory-limit or OOM events. Each service runs as UID 1000 with delegated CPU/memory/pids controllers, an 8 GiB parent limit, no swap, 600% parent CPU quota, a 64 MiB stack limit and a 180-second deadline. Server CPUs are 0,2,4,6; client CPUs are 8,10. The server leaf has a 1,024-task limit and four CPUs of quota. The glibc override applies only to the second private service and its children.

Use the [burst command](../../../docs/connection-queue.md) with `--memory-mib 2048` or `4096`; for the stack experiment add the stated `GLIBC_TUNABLES` value to that service's environment before starting Python. Each run needs its own output path and delegated service. Preserve failure exit codes rather than accepting the negative controls as capacity qualification.

[evidence.tar.gz](evidence.tar.gz) contains the three reports, console and cluster logs, exact probe/helper sources and the unchanged runtime build manifest. [manifest.json](manifest.json) hashes every member; all 16 regular members were read back and verified. Binaries and database directories remain local. This is WSL development evidence, not a sustained-load or representative Linux-server qualification.

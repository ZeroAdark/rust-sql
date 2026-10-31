# Allocator residency diagnostics

`pgrust_memory_accounts()` can report allocator residency only when Linux exposes
the anonymous mapping names used to identify mimalloc memory. When those names
are unavailable, the view omits `allocator_resident`, `allocator_retained` and
their companion `allocator_live` row. The reconciliation view also omits the
incomplete `mimalloc_resident` term. Process and cgroup resident counters remain
available, and `unattributed` includes the memory that cannot be assigned to
allocator retention.

Published peak charged-memory and PSS comparisons use kernel measurements, so
this diagnostic defect does not invalidate those results. The four PostgreSQL
memory and latency targets remain open.

Previously an empty list of named mappings became a zero mimalloc total. With
Hearth, only the independently measured pool was included. Both values could
understate allocator residency. The local WSL2 kernel
`6.6.87.2-microsoft-standard-WSL2` refuses the naming operation with `EINVAL`,
making it a reproducible negative case. Kernel version alone does not establish
availability; the runtime probes the actual operation described by the
[Linux naming API](https://man7.org/linux/man-pages/man2/pr_set_vma.2const.html).

The probe reserves one untouched page, attempts to name it, and unmaps it. A
successful probe or an unsupported-operation result is cached; subsequent
capability checks use one atomic load, O(1) time and constant space. Transient
failures leave the reading unavailable and allow a later diagnostic read to
retry. A failed read clears the previous reading's stored component values.
Supported hosts retain the existing linear mapping scan and page-residency scan.
Allocation, free and ordinary SQL execution receive no new tracking metadata.

The reclaim ladder retains its previous conservative estimate from identifiable
mappings plus the pool. That estimate can undercount mimalloc on a host without
mapping names. Kill thresholds and admission continue using their existing
process/cgroup measurements. Region residency is unavailable when either mapping
names or allocation tracing are unavailable.

## Local checks

```sh
cargo test --locked --release -j 2 -p memruntime --lib
```

The host-capability regression checks the syscall independently, verifies a
faulted named page on supported hosts, and requires missing totals and no stale
component values on unsupported hosts. The existing mapping-reader test verifies
the resident-page count using an owned mapping.

`scripts/validation/allocator_residency_probe.py` runs in an ordinary-user,
private systemd unit with delegated CPU, memory and PID controllers. Supply
`--bin-dir`, `--work-dir` and a new `--output` directory; `--baseline-bin` adds
the retained pre-fix runtime. Server/client CPU lists must be disjoint and
available. Each allocator gets a 2 GiB server limit, six ordinary clients, one
million analytical rows, durable updates and a restart check. The report records
kernel capability, binary/source hashes, effective allocator, SQL counters and
kernel resource counters before and after the workload and restart. These checks
qualify diagnostic behavior; performance qualification remains separate.

The [subsequent local comparison](../artifacts/validation/residency-performance-2026-10-30/README.md)
passes the provisional CPU/latency preservation guard at medium load across five
alternating repetitions per engine. CPU per bundle changes +0.33%; the roughly
1% memory difference does not establish a memory improvement. The entire capacity
campaign is excluded from performance acceptance because one PostgreSQL trial
recorded timed CPU throttling. Its correctness and restart checks still pass,
and all observations remain in the replayable evidence.

The retained binary also passes a [fresh compatibility and recovery qualification](../artifacts/validation/residency-runtime-2026-10-31/README.md):
231 reviewed SQL cases, 510 isolation cases, the differential, admission, connection,
idle-session and TLS probes, ten crash cycles with logical restore, and the independent
window oracle. Raw PostgreSQL expectations remain separate (203 passes, 28 documented
differences). These results bind to `02cfbca6`; they do not qualify later prototypes.

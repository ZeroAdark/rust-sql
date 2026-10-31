# Connection-queue validation

This package records the capacity-accounting and queue-data-structure fix in [the implementation notes](../../../docs/connection-queue.md). It is **not production qualification**.

- Candidate SHA-256: `2f35e3a361d2b660a73c5aefb739af6178ee3f4dfbd5b9b63f48b165e5d5ec3c`.
- Baseline SHA-256: `e7d3f26e5f0acfb3de5bd9b0971a4168cbfca2d3a25407c64658721c9e4ac20a`.
- Build base: `a4464503291ab566780af5b66f56a75631366d45`, plus the single runtime source file whose LF hash is in [manifest.json](manifest.json). The binary banner identifies this base because the build preceded the commit.
- Archive: [evidence.tar.gz](evidence.tar.gz), 2,590,282 bytes; SHA-256 `c7e2d1a1aa84bde9faedfb4ddd6c89cb8cf647bd591c0319025d64b06fad3545`.

The deterministic unit reproducer fails on the old implementation: a woken waiter is still alive, but the count becomes zero. The fixed implementation keeps it counted and rejects an overflowing entry. The old source plus reproducer and its assertion-failure log are preserved. All 26 current `lmgr_proc` unit tests pass, including the default parallel runner, and all 126 Python validator tests pass.

The same final protocol harness passes 12 contract checks on both binaries. Its full fixed-binary burst passes two waves of 800 ordinary startup connections: each holds 500 admitted and 300 queued, rejects one extra attempt, then drains successfully. All 1,600 intended startups succeed. There are no timeouts, hangups or leftover server processes. These checks complement the deterministic race reproducer; they do not reproduce the old race by themselves.

The final burst uses separate server/client cgroups and CPU sets, with a 4 GiB server memory limit, swap disabled and a 1,024-task limit. At each held phase, all 836 tasks have affinities within CPUs 0,2,4,6; the driver uses 8,10. Kernel-reported server peak memory is 2,573,369,344 bytes (2.40 GiB), with zero memory-limit, OOM or CPU-throttling events. Reports retain startup latencies, sampled RSS/PSS, cgroup counters, thread affinities, source hashes and service provenance. The parent service's journal memory value is not the server-leaf measurement. This short test establishes neither a CPU speedup nor safe operation at 1 or 2 GiB.

[checks.json](checks.json) also records 52 transaction-admission checks, 87 differential checks, 12 smoke checks, 510 isolation cases, ten whole-process crash/recovery cycles and logical restoration. The reviewed regression audit accepts 231/231; the raw 203/231 PostgreSQL comparison remains failed. The archive retains full raw suite outputs, reviewed audit, recovery/source/restored-cluster logs, build logs, unit logs and validation source snapshots. Final contract reports are in `contract/`; earlier runner logs are retained separately in `build/`. The earlier native control's complete outputs remain in the [validity-forwarding package](../aggregate-validity-2026-10-30/README.md), identified by this manifest.

All 1,323 regular archive members were read back and SHA-256 verified. Database files, binaries, build caches and ephemeral credentials are excluded. Reproduction commands and delegated-service requirements are documented in [connection-queue.md](../../../docs/connection-queue.md). CPU/performance calibration and the complete memory matrix must still be repeated for this binary before qualification.

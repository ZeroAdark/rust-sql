# Allocator selection evidence

**Keep the Hearth default for the CPU-first mixed workload.** Mimalloc reduces memory but increases CPU cost and fails the throughput and p99 gates. This package changes no runtime binary or allocator default and does not qualify production use.

The calibration uses the same binary on both sides: SHA-256 `2f35e3a361d2b660a73c5aefb739af6178ee3f4dfbd5b9b63f48b165e5d5ec3c`. Five interleaved pairs use a 4 GiB server cgroup, no swap, four server CPUs and separate client CPUs, six ordinary-role clients, six active transactions, one million fact rows, and 400 measured plus 50 warmup bundles per client. The server confirms the explicit allocator selection before measurement and after restart.

- CPU/bundle: 15,461.77 to 16,111.17 microseconds, **+4.20%**; all paired changes are positive, ranging from +3.69% to +4.99%.
- Throughput: 239.21 to 213.72 bundles/second, **-10.66%**.
- Aggregate p99: **+12.16%**; join p99 +10.80%. All query distributions are retained.
- Sampled peak PSS: 683.30 to 348.27 MiB, **-49.03%**. Sampled server cgroup memory decreases 50.13%.
- All 456,000 measured statements, 96,000 measured committed updates and ten restart checks pass. Restart verification also covers 12,000 warmup updates.

No calibration memory-limit, OOM or swap events occur. One mimalloc measurement records 1,171 microseconds of CPU throttling; all other measurements record zero. These short WSL distributions are not confidence intervals or a full memory matrix. [comparison.json](comparison.json) preserves every paired observation; [checks.json](checks.json) retains the failed performance gates. The [performance report](../../../docs/performance.md#allocator-selection-2026-10-30) documents the decision and reproducible service command.

The separate mimalloc connection pilot passes two 800-startup waves under a 2 GiB cap, peaking at 1,073,188,864 bytes. Its wrapper explicitly inserts the allocator selection after the cluster helper strips inherited `PG*` variables, and the server log confirms mimalloc. An initial environment-only attempt actually ran Hearth; its failed report and log are retained as an **invalid allocator control**, excluded from the calibration. The new cluster API verifies explicit selections through SQL, and the comparator rejects missing/wrong witnesses, allocator changes after restart and comparisons that change both allocator and binary. All 133 validator tests pass.

The residency experiment reads `/proc` mappings while two Hearth connection waves are held. Raw compressed `smaps`, grouped totals, reports and the exact observer are preserved. Its observation overhead excludes it from performance comparisons. Large anonymous mappings match the pool's chunk/guard geometry, an inference that guides further investigation rather than establishing complete allocation attribution. Database files, binaries and credentials are excluded.

[evidence.tar.gz](evidence.tar.gz) contains 74 verified regular members, 365,205 bytes, SHA-256 `b1474a30e3cf6591da93586eedd82e47ec5512fd32913048a56f4f2be80b519d`. [manifest.json](manifest.json) hashes every member. Sources, raw trial and cluster logs, unit output, service provenance, valid and invalid pilots, and residency captures are included. Runtime build provenance points to the earlier connection-queue binary; this change only extends validation and records evidence.

After extracting into a private directory, replay with Python 3 and psycopg installed:

```sh
python3 source/compare_resource.py --calibration \
  --input-dir calibration --source-dir calibration/source --output replay.json
cmp replay.json calibration/comparison.json
```

`--calibration` explicitly limits the result to 4 GiB. Without it, the verifier still requires all four 1/2/4/8 GiB limits. The replay was checked byte-for-byte. Targeted pool memory improvements and the remaining [production gates](../../../docs/production-readiness.md) remain open.

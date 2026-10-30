# Small-run page-policy experiment

Decision: **reject the runtime change because it increases connection-startup CPU.** Hearth and the current engine remain unchanged. The frame-test correction and focused Linux CI check are retained. This is WSL Linux x86-64 calibration evidence, not production qualification.

| Measurement | Baseline median | Candidate median | Change in median |
| --- | ---: | ---: | ---: |
| Mixed workload CPU, microseconds/bundle | 15,509.066 | 15,508.789 | -0.0018% |
| Mixed throughput, bundles/second | 238.007 | 238.898 | +0.37% |
| Mixed sampled peak cgroup memory, MiB | 666.688 | 648.383 | -2.75% |
| Connection-wave CPU, microseconds/accepted startup | 1,657.875 | 2,026.376 | **+22.23%** |
| First connection wave, seconds | 0.5294 | 1.3029 | +146.09% |
| Connection peak cgroup memory, MiB | 2,482.852 | 2,441.793 | -1.65% |

Each comparison has five interleaved pairs at 4 GiB with Hearth explicitly selected and witnessed. Server CPUs are 0,2,4,6; client CPUs are 8,10. CPU measurements use the private server cgroup, excluding the client driver and systemd service total. The mixed workload uses six clients, one million fact rows, 400 measured bundles and 50 warmup bundles per client, and `max_active_queries=6`. Its 456,000 measured statements and ten restart checks pass. All median query p99 changes stay within 5%, but CPU is flat and memory changes vary in direction across pairs.

The connection comparison completes 16,000 accepted startups and 20 overflow rejections: each trial has two waves of 500 admitted plus 300 queued ordinary connections, followed by a verified drain. Startup CPU increases in every pair, from +3.59% to +30.31%. Wave durations include client orchestration and diagnostic snapshots; they are not independent authentication-latency measurements. Measured phases have no CPU throttling. The successful trials have no memory-limit events, swap or leftover server processes.

Three candidate 2 GiB pilots pass both waves with peaks of 1,998.9336, 2,029.0898 and 1,985.25 MiB. A fresh unchanged-binary control at 2 GiB fails with an OOM group kill before completing a wave. The smallest candidate headroom is 18.9102 MiB; this does not establish a safe operating limit with active queries. The rejected policy is not enabled in the retained source.

## Sources and checks

- Source parent: `046beaa9bb2bc01baf5a54d58c543c0b7c1c13e2`.
- Baseline `postgres` SHA-256: `2f35e3a361d2b660a73c5aefb739af6178ee3f4dfbd5b9b63f48b165e5d5ec3c`.
- Experimental `postgres` SHA-256: `57abead9de0fe8f09490e2f809b7255b22ca7b845edd57eb95a1324d73a6fb73`.
- Experimental source patch, full changed-source snapshots, build manifest and driver are retained. Apply `candidate/candidate.patch` to the source parent for that experiment; it is not a patch for the current branch head.
- The original cold-run test fails. Advice after the first write still fails its residency check. The complete experiment passes, including contents and advice restoration, and its normal allocator suite passes 113 tests with one helper ignored by design.
- The old frame test also fails on the unchanged code. The retained repair checks residency before use and excludes allocation bookkeeping from the touch fault count. All 112 normal allocator tests pass on the retained source; one helper is ignored by design. The focused CI command also passes without the local enlarged test-thread stack setting.
- The connection analyzer rejects altered binary identities, memory limits and accepted-startup counts. Both comparison outputs replay byte-for-byte from the archive. No full regression/isolation/crash campaign was run for the rejected binary.

`evidence.tar.gz` is 712,190 bytes, SHA-256 `cdbf497d79d6b5ee817e816b55d60d539e2e830a981906a69d062e0c1ce389fb`. It contains 155 regular files (7,040,300 uncompressed bytes): raw reports, cluster logs, driver sources, journals, comparison sources, candidate patch and snapshots, and successful and failing test logs. `manifest.json` hashes every member. Database files, executables and credentials are excluded.

## Portable replay

Extract the archive into a new directory, then run with Python 3.12:

```sh
python3 mixed/source/compare_resource.py --calibration \
  --input-dir mixed --source-dir mixed/source --output mixed-replayed.json
cmp mixed-replayed.json mixed/comparison.json
python3 source/compare_small_pages_connections.py \
  --input-dir connection --output connection-replayed.json
cmp connection-replayed.json connection/comparison.json
```

The analyzers validate recorded hashes, limits, allocator witnesses, trial completeness, counters and results. Replaying evidence does not rerun a server or establish independent build reproducibility. New live measurements require separately built binaries and delegated cgroups; the recorded systemd commands and driver sources preserve this experiment's setup.

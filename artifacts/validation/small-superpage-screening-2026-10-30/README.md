# Small-object superpage experiment: rejected

This candidate saves memory but fails the requirement to preserve the retained runtime's performance. All seven experimental source files are restored in both checkouts. The retained executable is still `8e5bfc2b093055a5d2257dc817037a2b2f2906faf7c1b83e431f472a2f0513f0`; no allocator or configuration default changes are retained.

The prototype gives small-object runs their own placement class, prepares base pages once per 2 MiB superpage, and restores the huge-page preference when that superpage becomes wholly free. Bounded partial-list searches still lend across classes when a fresh claim is refused. Object allocation and free retain their constant-time paths. The added checks exercise both borrowing directions at the cap, payload preservation and page-advice restoration. However, the synchronous preparation extends an allocator critical section.

## Matched mixed-workload results

Five interleaved triples per phase compare native PostgreSQL 18.6, the retained Rust SQL executable, and the candidate. The original six-client, 19-statement bundle, durable commits, unpaced ten-bundle warmup, CPU affinity and 2 GiB server limit are unchanged. Medium load offers 9.4 bundles/second; capacity is unpaced. These are short WSL2 Linux x86-64 observations, not representative-server or production qualification. Values below are medians of per-trial measurements; latency values are medians of each trial's p99.

| Medium-load metric | PostgreSQL | Retained Rust SQL | Rejected candidate |
| --- | ---: | ---: | ---: |
| CPU ms/bundle | 197.059 | 17.724 | 17.787 |
| Sampled charged-memory peak, MiB | 223.715 | 483.758 | 443.406 |
| Sampled process PSS peak, MiB | 93.257 | 511.346 | 470.472 |
| Read p99, ms | 0.396 | 0.498 | 0.514 |
| Update p99, ms | 1.189 | 1.283 | 1.284 |
| Aggregate p99, ms | 30.567 | 4.450 | 4.239 |
| Range p99, ms | 12.453 | 1.092 | 1.096 |
| Join p99, ms | 45.597 | 5.623 | 5.604 |

Medium charged memory/PSS decrease 8.34%/7.99%, but read p99 increases 3.29%; update p99 is essentially unchanged. At capacity, charged memory/PSS decrease 7.55%/7.19%, CPU changes -0.20%, and throughput decreases 1.41% (4168.00 to 4109.23 statements/second). Capacity bundle p99 increases **5.66%**, from 37.926 to 40.073 ms, exceeding the existing 5% screening guardrail. Aggregate/range/join p99 increase 3.20%/3.46%/2.45%. These observations do not establish causal or statistically significant improvements. The original PostgreSQL comparison remains a separate result; none of the four target gaps is closed here.

## Connection-burst rejection

Five alternating pairs use the retained and candidate executables, the same four server CPUs and a 4 GiB limit. Each trial has two waves of 800 accepted startups: 500 initially admitted, 300 queued, and one additional startup correctly rejected per wave.

| Metric | Retained | Candidate | Change |
| --- | ---: | ---: | ---: |
| Server CPU per accepted startup, ms | 1.2105 | 10.0016 | **+726.24%** |
| Peak charged memory, MiB | 1940.430 | 1630.316 | -15.98% |
| First-wave duration, seconds | 0.427 | 3.743 | +777.13% |

Every pair uses more CPU with the candidate. Wave duration includes client orchestration and diagnostic snapshots; it is not standalone authentication latency. CPU covers the server cgroup between the same before/after snapshots. The memory saving does not justify this regression.

## Checks and diagnostic limits

- All 30 mixed trials pass exact results and committed-state/restart checks: **136,800 timed statements**, 34,200 warmup statements, 30 restarts and 60 normal stops. No OOM, swap, memory-limit or CPU-throttling event is recorded.
- All ten connection trials pass: **16,000 accepted startups**, 20 overflow rejections, complete queue draining, no lost startup, and empty private server cgroups afterward.
- All **117 allocator tests pass**, with one existing ignored test. The prototype's new checks are within the existing sequential kernel test.
- The differential Bind/protocol probe passes all eight groups against native PostgreSQL, including 192 committed updates per engine, malformed input cleanup, rollback, cursor lifetime and plan invalidation. The broad 231-case/510-case qualification suites were not rerun on this rejected prototype.
- Separate diagnostic executables retain byte-identical `.text` sections to their respective timed binaries. The candidate capture has 251 user-space samples: 157 in `sched_yield`, with 64.94% inclusive samples under allocator `run::carve`. This is consistent with refill contention; it is an inference, not complete kernel-cost attribution. Short-lived threads can exit before a sample is taken, the retained capture has only 21 samples, and some unwinds are incomplete. No performance claim uses the profiled runs. The first capture attached only to the idle supervisor and had no samples; it is retained and explicitly excluded.

The frozen prototype is based on `3594c736fd141d37333c16d039a050fcfed386a2`, with patch SHA-256 `df2ed3847cb7c28a13a9ad5579aeb764ecb6f2614f79138f79bd30d158763a69` and executable SHA-256 `c01c29f2f75102699fda0d9799b4fc68c3bcad480a90ecadc1d641f32c43ec28`. Its source, build/test logs, rejection decision and verified restoration record are archived. The local `target/release/postgres` was also restored by copying the previously qualified retained executable; this is not represented as a new build.

`evidence.tar.gz` contains the raw trials, effective settings, plans, harnesses, cluster logs, prototype source and diagnostic reports. Raw perf captures remain local; their hashes and sizes are archived. `results.json` preserves medians and ranges; `connection-results.json` preserves every pair. The standard-library replay reads archive members without extracting them and checks hashes, identities, counts, settings, durable state, resource limits and summaries:

```sh
python3 artifacts/validation/small-superpage-screening-2026-10-30/replay.py
```

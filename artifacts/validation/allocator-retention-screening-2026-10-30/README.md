# Allocator retention screening (2026-10-30)

These are rejected configuration screens and diagnostic profiles, not accepted runtime changes. All timed runs use the constructor-fixed `8e5bfc2b093055a5d2257dc817037a2b2f2906faf7c1b83e431f472a2f0513f0` executable. Native PostgreSQL remains the reference in each alternating pair. The medium workload, durability, six clients, 9.4 bundles/second and four server cores match the original comparison.

| Rust SQL setting | CPU ms/bundle | Charged MiB | PSS MiB | Read p99 ms | Update p99 ms | Aggregate p99 ms | Join p99 ms |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| Earlier 16 MiB maximum, 5 pairs | 17.561 | 458.82 | 485.06 | 0.511 | 1.264 | 4.219 | 5.488 |
| Parallel workers per gather = 0, 16 MiB maximum, 3 pairs | 16.438 | 415.95 | 443.64 | 0.488 | 1.235 | 6.362 | 10.573 |
| Requested warm reserve and maximum = 0, 3 pairs | 17.780 | 460.59 | 484.99 | 0.543 | 1.304 | 4.635 | 6.080 |

Reducing parallelism saves memory and CPU in this screen, but aggregate and join p99 worsen substantially relative to the earlier Rust SQL measurements. It fails the requirement to preserve those gains. Requesting no warm reserve does not materially reduce memory and yields worse latency measurements. Neither setting is selected. These three-pair screens are not a precise estimate of small performance differences; raw PostgreSQL controls, every trial, and the separate short serial pilot are retained.

The isolated memory profiles collect allocator owner counters and `/proc` mapping residency. They are diagnostic runs, excluded from CPU and latency evidence. After five idle seconds in the mixed profile, charged memory is about 228 MiB and anonymous PSS about 175 MiB; executable text/readonly mappings contribute another roughly 57 MiB PSS. Allocator capacity counters are not resident-memory measurements. The later per-mapping profile localizes most anonymous memory to the pool reservation.

`python3 replay.py` verifies every archived byte, the 14 timed-run correctness/restart/resource results and the exact screening summaries. The evidence also retains harness copies, settings, plans, process exit codes, server logs and command receipts. These results do not establish production readiness.

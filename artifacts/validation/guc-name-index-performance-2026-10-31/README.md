# Shared configuration-name index: performance evidence

The candidate shares the immutable built-in configuration-name index independently of thread retention. Per-session values, hooks and custom names remain private. Retain this change locally: the medium, capacity and connection-startup preservation checks pass, and memory falls in each scope. Lookup remains expected O(1); per-registry setting initialization remains linear in the number of settings.

These are five interleaved repeats with PostgreSQL 18.6, the retained Rust binary `02cfbca6`, and candidate `57baa2ef`. SQL results, protocol types, committed updates, restart persistence, durability, allocator identity, resource limits and cleanup are checked. The mixed workload has one million fact rows and six clients, with 2 GiB per server. Both Rust builds use Hearth.

| Candidate versus retained Rust | Medium, fixed rate | Capacity, unpaced |
| --- | ---: | ---: |
| CPU per bundle | +0.13% | -0.69% |
| Throughput | Fixed offered rate | +0.07% |
| Peak charged memory | -1.61% | -2.17% |
| Peak process PSS | -1.41% | -1.79% |
| Read p99 | -4.04% | -0.73% |
| Update p99 | +0.75% | +0.88% |
| Aggregate p99 | +1.93% | +4.49% |
| Range p99 | +0.56% | -8.72% |
| Join p99 | +1.43% | +1.23% |
| Bundle p99 | -2.33% | -1.74% |

All CPU/throughput/p99 changes remain inside the existing 5% guards. Capacity aggregate p99 is near that guard; this is a scoped acceptance, not a claim of universally unchanged latency. Including warmup and setup does not reveal deferred Rust CPU cost. Medium PostgreSQL warmup throttling is recorded outside the unthrottled timed window.

The original quota-controlled capacity campaign has three throttled PostgreSQL trials and is rejected as performance evidence. All 15 raw trials remain archived. A separate complete capacity campaign uses hard kernel CPU sets: four physical server cores and two separate driver cores, no CFS quota. A preflight proves neither process can escape its assigned CPU set. No timed server or driver throttling occurs in either accepted campaign. The two resource-control methods are documented separately.

Five paired 4 GiB startup trials accept 16,000 connections and reject 20 over-capacity attempts correctly. Candidate CPU per accepted startup falls 5.91% (6.14% including shutdown), and median peak charged memory falls 8.33%. Both-wave duration falls 5.67%, but it includes client orchestration and diagnostic snapshots. Three further candidate runs at 2 GiB accept 4,800 startups with no OOM, swap, memory-limit event or unclean exit; worst peak is 1745.09 MiB, leaving 302.91 MiB of headroom.

The four original PostgreSQL memory/latency targets remain open. In the accepted medium run the candidate uses 474 MiB charged memory / 502 MiB PSS versus PostgreSQL's 224 / 93 MiB; read p99 is 0.497 versus 0.407 ms, and update p99 1.277 versus 1.194 ms. Larger CPU and analytical gains remain, but this is a small memory improvement.

Run `python3 replay.py` to verify the archived evidence, source identities, trial coverage, timing validity and preservation guards without running a server or extracting archived code. Forged CPU bindings and shutdown exits are rejected. See the adjacent [runtime qualification](../guc-name-index-runtime-2026-10-31/README.md) for the build, unit/negative controls and full SQL, isolation, protocol, security and recovery evidence. These local WSL results do not qualify a production release. GitHub Actions remained disabled.

# Configuration registry allocation: performance evidence

Retain candidate `0be60915` provisionally after the longer capacity follow-up and full runtime checks. The original short capacity test fails its range-p99 guard and remains visible; there is no claim that every original guard passed. The reference is accepted Rust binary `57baa2ef`, with native PostgreSQL 18.6 (`07b16e1a`) in the medium and initial capacity comparisons. Both Rust builds use Hearth.

All mixed-workload campaigns use one million fact rows, six ordinary clients, durable commits, four physical server cores, two separate driver cores, and 2 GiB per server. Hard kernel CPU sets and an affinity-escape preflight enforce isolation without CFS quota. There is no timed server or driver throttling, swap, OOM or memory-limit event. Every trial verifies SQL values, type IDs, committed updates, persistence across restart and clean shutdown.

| Candidate change versus accepted Rust | Medium fixed rate | Short capacity | Longer capacity |
| --- | ---: | ---: | ---: |
| CPU per bundle | +0.53% | +1.01% | -0.03% |
| Throughput | Fixed offered rate | -1.15% | -0.17% |
| Peak charged memory | -11.25% | -5.76% | -3.75% |
| Peak process PSS | -10.19% | -4.75% | -3.56% |
| Read p99 | +0.37% | +0.61% | +0.72% |
| Update p99 | +0.26% | -2.62% | +1.25% |
| Aggregate p99 | -0.53% | -5.67% | -0.09% |
| Range p99 | +3.85% | **+5.27%: fails** | +2.56% |
| Join p99 | +2.76% | -2.24% | +0.01% |
| Bundle p99 | +1.24% | +3.38% | +1.39% |

Each column summarizes five interleaved repeats per engine. The medium and short capacity phases contain 15 trials each; the longer phase contains ten Rust-only trials. The latter investigates the borderline range-latency miss, not a new PostgreSQL comparison. Its design was fixed before execution: 1,500 bundles per client rather than 50, unchanged 5% guards, and all five pairs retained. Timed Rust windows increase from about 1.4 seconds to 38–42 seconds. The longer run does not reproduce the guard crossing, but range p99 remains higher. One faster baseline repetition is retained; medians do not imply identical performance in every pair. Including setup and warmup does not reveal deferred CPU cost. Together the campaigns check 1,846,800 timed SQL statements.

| Medium-load metric | PostgreSQL 18.6 | Candidate |
| --- | ---: | ---: |
| CPU per mixed bundle | 194.76 ms | 17.50 ms |
| Peak charged memory, including cache | 223.86 MiB | 437.42 MiB |
| Peak process PSS | 92.82 MiB | 464.82 MiB |
| Point-read p99 | 0.414 ms | 0.488 ms |
| Committed-update p99 | 1.192 ms | 1.275 ms |
| Aggregate p99 | 30.315 ms | 4.042 ms |
| Range p99 | 11.836 ms | 1.067 ms |
| Join p99 | 45.318 ms | 5.617 ms |

The initial unpaced comparison delivers 362.14 versus 4069.71 statements/second. CPU savings remain about 91% at medium load. The four original PostgreSQL memory/read/update targets remain open. These narrow, highly compressible data favor columnar analytics and do not establish pure-OLTP or language-wide performance. Charged memory and PSS have different accounting domains and are not additive.

Five interleaved 4 GiB connection pairs accept 16,000 startups and correctly reject 20 over-capacity attempts. CPU per accepted startup falls 13.40% (11.76% including shutdown); peak charged memory falls 13.36%, from 1783.45 to 1545.10 MiB. Every pair uses less CPU and memory. First-wave duration falls 17.79%, but includes client orchestration and diagnostics rather than pure authentication latency. Three additional candidate-only 2 GiB trials accept 4,800 startups without OOM, swap, memory-limit events or unclean shutdown; worst peak is 1546.56 MiB, leaving 501.44 MiB headroom.

Run `python3 replay.py` to check exact binaries, frozen sources, all 40 mixed trials, connection accounting, startup pilots, guards and the preserved short-run failure. Forged CPU bindings and shutdown exits are rejected; optimized Python is refused. Replays verify recorded evidence and do not run archived code or rerun SQL. See the adjacent [runtime evidence](../guc-registry-reserve-runtime-2026-10-31/README.md). These local WSL results do not qualify a production release. GitHub Actions remained disabled.

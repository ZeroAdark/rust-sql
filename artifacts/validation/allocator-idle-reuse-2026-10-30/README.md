# Idle allocator reuse evidence

The final change shares current runs after connection setup and through existing idle timers, keeps empty runs formatted, and makes synchronous pressure release see abandoned empty runs. General allocation has no added handoff bookkeeping. [Implementation and bounds](../../../docs/idle-allocator.md) describe the fixed class-slot scan and linear remote-free/pressure work.

Status: development validation only; **not production qualified**. CPU has priority over memory.

## Final measurements

| Limit | CPU/bundle | Throughput | Largest p99 change | Peak cgroup memory | Peak PSS |
| --- | ---: | ---: | ---: | ---: | ---: |
| 1 GiB | -0.47% | -0.47% | -0.57% | -0.04% | +1.86% |
| 2 GiB | +1.71% | -1.30% | +2.46% | +1.01% | +1.62% |
| 4 GiB | +1.24% | -1.03% | +3.02% | -0.39% | -0.95% |
| 8 GiB | -1.09% | +0.52% | +4.25% | +0.67% | +2.44% |

Forty interleaved trials execute 1,824,000 measured statements plus warmups. Both binaries explicitly use Hearth, six ordinary-role clients, one million fact rows, 400 measured bundles and 50 warmup bundles per client, a six-transaction cap, four server CPUs and durability enabled. All results, committed updates and clean restarts pass; no OOM, swap, memory-limit or measured CPU-throttling event occurs. Every median throughput/p99 change meets the provisional 5% guardrail. The table uses ratios of independent medians; JSON retains every paired change. No general additional CPU speedup is claimed.

Five separate 4 GiB connection pairs complete 16,000 accepted startups and 20 overflow rejections. Median peak cgroup memory changes by -21.59%; startup-phase CPU per accepted startup changes by -19.03%, and CPU through server shutdown by -12.95%. The latter paired range is -28.47% to +24.73%. These short, variable runs do not establish a reliable CPU saving. Four server CPUs are affinity-checked for every held task, with a five-CPU quota on both sides to avoid CFS period-boundary throttling. Server and parent CPU counters are complete and show no throttling, including server teardown. This quota differs from the mixed matrix and is recorded in metadata. Wave times include orchestration and diagnostic snapshots, not just authentication.

Three 2 GiB pilots each complete two waves of 500 admitted plus 300 queued connections. Peaks are 1822.87, 1864.38, 1852.75 MiB, leaving at least 183.62 MiB (8.97%). These pilots do not qualify 500 concurrent active queries, long churn or a production service. Previous baseline 2 GiB OOM failures remain retained.

## Correctness and source identity

The final binary passes 117 allocator tests (one helper intentionally ignored), 12 smoke checks, 510 isolation cases, 231/231 reviewed SQL cases, 87 differential checks, 52 admission checks, 12 connection-contract checks, 12 idle-session checks and 21 TLS/authentication checks. Ten whole-process crash cycles and logical dump/restore checks pass. Raw PostgreSQL expectations still report 28/231 mismatches; that profile remains separate. The independent window oracle passes 220 comparisons on each of the candidate and separately patched PostgreSQL. The PostgreSQL EXCLUDE TIES bug difference remains intentional.

Final `postgres` SHA-256: `5c1049ca3dd5d17fb91c7cca088808c4afce8236f62fa1807bcd094d7696555a`. Unchanged baseline: `2f35e3a361d2b660a73c5aefb739af6178ee3f4dfbd5b9b63f48b165e5d5ec3c`. Source parent: `f309435542dd52cc3b6fe423d1dd76a062c42e97`. Runtime snapshots, patch, build commands and LF-normalized hashes are archived; this is not an independently reproduced build. The focused CI command passes six tests with default thread-stack settings.

The final idle-session probe requires timer witnesses after retained-object setup, then checks prepared statements, transaction cursors, held cursors and transaction state after resumption. The 64-thread live-allocation negative control retains 12 MiB; sharing uses 256 KiB. The pressure negative control retains 12 MiB after exited idle threads until an explicit sweep; the fix reclaims it synchronously. Forced-reservation-failure controls cover both general and cache allocations, payload preservation, thread exit and remote frees. Negative versions reproduce retention; the final versions pass without a background purger.

## Experiment history

| Stage | Disposition |
| --- | --- |
| `idle-handoff` | Immediate empty-run return rebuilt 600 runs in 200 idle cycles, raised mixed median CPU 1.06%, and failed join p99. Its startup dataset has a CFS throttle event and is not accepted as a CPU comparison. |
| `idle-reuse` | Kept empty runs formatted. Mixed matrix guardrails passed, but a lifecycle test exposed a synchronous release estimate that skipped abandoned empty runs. The existing background sweep still reclaimed them. |
| `idle-reclaim` | Fixed immediate reclamation and duplicate sweeps. Correctness passed, but every-command handoff failed 4 GiB range p99 (+5.29%). |
| `idle-boundary` | Moved handoff to setup/idle timers. All throughput/p99 guardrails passed; 1 GiB independent median CPU rose 1.62% (paired median +0.36%). Superseded to remove allocation bookkeeping. |
| `idle-cold` | Removed allocation bookkeeping, but failed the 2 GiB range p99 guardrail (+13.04%) and new fallback lifecycle controls. |
| `idle-safe-final` | Final implementation: pool-backed run handoffs, immediate retirement of empty system cache runs, and remote-free flushing when the pool is off. |

Separate instrumented 1/4 GiB workloads record zero R1 pressure-release calls. They are diagnostic evidence, not performance comparisons. The initial reuse startup-driver import failure and a root-user CI-command failure occurred before their respective tests and remain archived; corrected runs are separate. An intermediate `idle-safe` build was stopped before producing a binary when the extended cache fallback control failed; its source and logs are retained. Earlier idle-probe sources are retained with their matching report hashes. Boundary startup analysis retains its original version before adding shutdown CPU and stricter counter checks; raw trial records were unchanged.

## Archive integrity and replay

| Archive | Bytes | Regular files | SHA-256 |
| --- | ---: | ---: | --- |
| `experiments.tar.gz` | 5,995,827 | 2,635 | `d2ecf079431bff6927ed6dd72fe6283655bdf2e3e9e2ae7e31358bb291782b5e` |
| `followups.tar.gz` | 9,492,481 | 3,940 | `064de0b40450b18ec3c311202f84ba63d0d53df38a97a3cf80a615bc88b94a33` |
| `cold.tar.gz` | 5,140,924 | 2,464 | `6737f316b9380fd85d059ee66fd2060d69795ade71ca2219b988ab4a33dfccc7` |
| `evidence.tar.gz` | 9,550,814 | 3,203 | `81da248db9a3f540fd6315650f4d7bba4c261d864db7baee6acea792d0480551` |

`manifest.json` hashes every regular member. Archives contain reports, complete SQL outputs, native results, pinned expectations and the required timezone data for the final audit, source snapshots, runners, journals and server logs. Database files, executables, TLS private keys and live credentials are excluded.

From a trusted checkout with Python 3.12 or newer:

```sh
python3 artifacts/validation/allocator-idle-reuse-2026-10-30/replay.py
```

Replay was verified with Python 3.12 on Linux and Python 3.14 on Windows: 12,242 archived files, seven mixed comparisons, five startup comparisons and all 231 reviewed cases. The archived timezone data and UTF-8 mode keep this independent of host defaults.

Replay verifies archive/member bytes, recomputes seven mixed-work comparisons and five valid startup comparisons, and reruns all 231 reviewed SQL cases against archived native results and pinned policies. It starts no server and does not reproduce binaries or live measurements. New measurements require the recorded build/delegated-cgroup setup and separately frozen binaries. This remains WSL x86-64 evidence; production Linux hardware, sustained workloads and remaining [qualification gates](../../../docs/production-readiness.md) are still required.

# Idle allocator reuse

Hearth previously kept a partially filled current small-allocation run private to each backend and size class. Its existing idle handoff covered cache runs only and was not wired into the command loop. Large populations of idle or queued connections could therefore retain substantial capacity above live allocation charges.

The command loop now calls the handoff once after connection setup, before accepting commands. Later handoffs run through the existing idle-release/passivation timer after any sliced tail walk finishes. Busy sessions keep local current runs between commands. The existing timer settings still control later idle work: `pgrust.memory_release_idle` defaults to 1,000 ms and `idle_passivate_timeout` to 60 seconds; setting both to zero disables those later handoffs. The allocator detaches its current general and cache runs, flushes pending remote frees, and publishes detached runs through its existing adoption mechanism. Live allocations retain their addresses and contents. Another thread can use the remaining slots; the original thread can resume and free its allocations even if ownership of the run has changed.

Only runs inside the pool reservation are shared. System-backed fallback runs stay local, so empty runs are freed on thread exit even when pool initialization failed and no background purger can start.

Empty runs stay formatted for reuse. Returning every empty run to the pool at every command boundary rebuilt free lists repeatedly: the first candidate increased median mixed-workload CPU by 1.06% and failed a query p99 guardrail. A focused test reproduced 600 fresh runs across 200 idle cycles and three size classes; the retained reuse policy needs at most three.

An intermediate candidate shared runs after every ReadyForQuery. Its final matrix had slightly higher CPU use and a 4 GiB range-query p99 increase of 5.29%, outside the 5% guardrail. A separate diagnostic recorded no pressure-release calls during that workload. The revised integration moves ownership transfers to setup and actual idle periods.

The background purger already sweeps adoption lists periodically. Synchronous pressure release now also collects these runs **before** calculating releasable bytes. Otherwise a zero-byte estimate could return early while empty runs waited for the background sweep. The background and cache-reclaim callers avoid redundant traversals, and the purger handles an ask arriving after its initial sweep decision. A zero-byte release request remains a no-op. Nonzero synchronous requests still flush remote fallback frees when the global allocator is active but the pool is off. Existing R1 call/step timers measure the release phase, excluding the preflight sweep and lock wait.

## Work and storage bounds

- An idle handoff examines the fixed 24 general and 24 cache current-run slots. It does not walk the thread's growing partial or full lists.
- Publishing a run uses the existing atomic adoption stack. The existing allocation path bounds adoption and full-list searches; the ordinary per-object fast path gains no new store. General allocation also gains no per-run handoff bookkeeping: its fixed current-run slots are inspected at the handoff itself.
- Collecting remote frees is linear in the collected blocks. Explicit pressure reclamation can scan the abandoned runs; this work is outside the ordinary per-object allocation path. Reuse does not require per-run page-advice system calls.

The pool's existing `pgrust_memory_owner_end()` diagnostic adds `pool_heap_capacity`, `pool_small_capacity`, and `pool_cache_capacity`. These are checked-out capacity by tag, not resident bytes. Together with live allocator charges, they explain `pool_small_slack`; cached residency readings have a separate age and must not be treated as an atomic RSS snapshot.

## Verification and limits

The server probe checks prepared statements, transaction cursors and held cursors after both timer stages, with per-session log witnesses. The focused allocator tests cover concurrent adoption, original owners resuming, remote frees, reuse of formatted empty runs, immediate pressure reclamation after idle threads exit, and general/cache fallback lifecycles after a failed pool reservation. The unchanged allocator retains 12 MiB of current-run capacity in the 64-thread live-allocation reproducer; the handoff uses 256 KiB. A separate negative control leaves 12 MiB outside synchronous warm release until an explicit adoption-list sweep; the pressure fix closes that gap.

See the [resource measurements](performance.md#idle-allocator-reuse-2026-10-30) and [complete evidence](../artifacts/validation/allocator-idle-reuse-2026-10-30/README.md). The evidence separates mixed-workload CPU and latency, connection-pressure memory measurements, superseded and rejected candidates, and checks of the final binary. It does not establish production readiness, a universal CPU speedup, or safe active-query capacity for an arbitrary number of connections.

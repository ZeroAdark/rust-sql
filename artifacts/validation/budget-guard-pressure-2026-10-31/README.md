# Rejected budget-triggered reclaim experiment (2026-10-31)

**Rejected, runtime source reverted.** The retained executable remains `0be60915`; experimental `73addf2f`
is not promoted. Its 118 allocator tests pass (one ignored), and disabling the new budget predicate makes the new
test fail. The constrained-memory SQL campaign nevertheless fails five of six candidate trials. Performance
comparisons are skipped because pressure qualification fails.

The candidate enables inline free-page reclaim when the optional ladder or allocator budget leaves insufficient
room. Its default path has a constant scan budget: up to eight whole 2 MiB pages, at most 64 bitmap words searched
per free-state attempt, with cursor advancement. It does not scan every partial allocation inline. Existing ladder
behavior remains separate. It neither makes global allocation fallible nor deliberately releases live allocations.

## Recorded results

Twelve interleaved trials use the same one-million-row wide-sort workload as the
[preceding investigation](../sort-pressure-2026-10-31/README.md), 1 GiB server cgroups, zero swap and disjoint hard
CPU sets. Each requests twelve alternating 64 KiB serial / 16 MiB parallel sorts. Full-result digests, physical
spills, cleanup, connection recovery and full ordered data after restart are checked.

| Arm | Passing trials | Watchdog cancellations | Trials with OOM group kill |
|---|---:|---:|---:|
| PostgreSQL 18.6 | 3 / 3 | 0 | 0 |
| Retained Rust, ownership observer | 0 / 3 | 3 | 0 |
| Candidate, no ownership observer | 1 / 3 | 1 | 1 |
| Candidate, ownership observer | 0 / 3 | 2 | 1 |

There are 93 successful main sorts, six watchdog cancellations and two OOM failures. Both OOM trials restart and
verify all acknowledged rows with the complete ordered digest; their first server exits are still recorded as -9.
All twelve trials verify full ordered data after restart. PostgreSQL peaks around 691 MiB; every Rust trial reaches
the 1 GiB lifetime cgroup limit. Failures do not count as useful throughput. These pressure timings are diagnostic,
not performance qualification; this campaign does not prove that the candidate caused the underlying OOM risk.

## What changed the next step

All sampled candidate observer counters report the guard enabled but **zero reclaim calls and bytes**. The
implementation's trigger is therefore ineffective in this reproducer despite its focused tests passing.

The existing memory policy reserves at least 1 GiB for the OS before calculating its budget. A separate compiled
Rust diagnostic confirms that 512 MiB and 1 GiB limits both produce budget zero, while 2 GiB produces 896 MiB.
The allocator interprets cap/budget zero as unlimited. This is a concrete gap between the synthetic positive-budget
test and the production path. The next revision must distinguish a known exhausted budget from an unknown limit,
publish usable reclaim guidance early, and prevent optional prefaulting from immediately consuming recovered space.
No such fix is claimed by this archive.

Run `python replay.py` with assertions enabled. It verifies hashes, frozen sources and binaries, bounds, all
successful results, restart checks, failed runs, zero-reclaim observations and the separate budget diagnostic.
An intentional removal of an OOM event must fail verification. Replay checks recorded evidence without rerunning
SQL. The raw reports, commands, logs, candidate patch and tests remain in `evidence.tar.xz`. No GitHub Actions ran.

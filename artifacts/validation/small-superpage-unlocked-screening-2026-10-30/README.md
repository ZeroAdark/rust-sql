# Small-object page preparation outside allocator locks

Rejected; all eight experimental source files are restored to `fb73916ad1d5477fdfeef7d8b25c977249032b5e`. Runtime and defaults are unchanged. The target executable is explicitly restored from the retained `8e5bfc2b...` binary, not represented as a new build.

The prototype keeps a newly claimed superpage exclusively Busy while releasing the class and runs locks for OS page preparation. It reacquires the locks before publishing partial-page and run metadata. The partial-list searches remain bounded, with no new per-object bookkeeping. A deterministic two-thread check verifies that a second allocation proceeds while the first preparation is paused, cannot use the in-flight page, and preserves ownership during purging. Moving the pause back under the locks makes that check fail as expected. All 117 allocator tests pass (one existing helper ignored), the release build passes, and all eight protocol groups match PostgreSQL 18.6, including 192 committed updates per engine.

The first five-pair connection campaign passes correctness but fails the strict performance-validity rule: baseline trial 4 records 613 microseconds of CPU throttling and candidate trial 5 records 38,685. Every trial is retained; no subset is selected for performance claims. A complete second campaign passes the resource-validity checks.

| Valid second campaign: five alternating pairs | Retained runtime | Prototype | Change |
| --- | ---: | ---: | ---: |
| Median server CPU per accepted startup | 1,402.82 us | 1,752.81 us | +24.95% |
| Median peak charged server memory | 1,882.97 MiB | 1,632.03 MiB | -13.33% |
| Median first-wave orchestration time | 0.45905 s | 0.53012 s | +15.48% |

Every valid pair uses more CPU. The memory saving therefore fails the CPU-preservation requirement. These timings include the burst harness and snapshots; they are not standalone authentication latency. Each trial uses two waves of 500 initially ready plus 300 queued clients, four server CPUs and a 4 GiB memory limit. Each campaign completes 16,000 accepted startups and 20 expected overflow rejections, with normal shutdowns and no remaining server processes. No medium/capacity SQL comparison was run for this rejected variant.

[The validated comparison](connection-results.json), [first-campaign rejection](first-campaign-validation.json), [candidate identity](candidate-manifest.json), [decision](decision.json), and [restoration record](restoration.json) accompany the [106-member evidence archive](evidence.tar.gz). It contains both complete campaigns, frozen source and build/test logs, the negative control, protocol checks and the initial test-hook failure. `python3 replay.py` verifies every member hash, rejects the throttled first campaign, validates the second and checks the source/test/restoration records without extracting or executing archived contents. Replay passes on Linux/Python 3.12 and Windows/Python 3.14.

All four PostgreSQL memory and p99 targets remain open. The retained checkpoint independently passes [all three Linux CI jobs](https://github.com/ZeroAdark/rust-sql/actions/runs/41150048120). Neither this experiment nor that CI pass establishes production qualification.

# Faster warm-page reclamation: 100 ms and 200 ms screens

Both settings are rejected as defaults. These experiments use the same retained `8e5bfc2b...` server executable in both Rust arms. Only the candidate receives `pool_warm_zero_max=16777216` plus `pool_decay_ms=100` or `200`; every trial records and checks the live server environment. Runtime source and defaults are unchanged.

Each campaign has five alternating PostgreSQL / retained Rust / candidate triples: one million fact rows, six ordinary-role clients, four server CPUs, two separate client CPUs, a 2 GiB memory limit, no swap, 128 MiB shared buffers, 16 MiB work memory, two gather workers and durable commits. Each bundle contains twelve point reads, four updates, one aggregate, one 4,096-row range and one join. Medium load uses 30 measured bundles per client at 9.4 bundles/second total; capacity uses 50 unpaced bundles. Both retain ten unpaced warmup bundles per client, with no added cooldown. Values below compare medians across the five trials in each arm; p99 values are medians of trial p99s.

| Candidate change against its contemporaneous retained control | 100 ms, medium | 200 ms, medium | 200 ms, valid capacity repeat |
| --- | ---: | ---: | ---: |
| Timed server CPU per bundle | +1.65% | +0.38% | -1.04% |
| Peak charged memory | -21.03% | -20.37% | -15.99% |
| Peak process PSS | -19.96% | -19.03% | -15.59% |
| Point-read p99 | +2.48% | -3.36% | +3.61% |
| Committed-update p99 | +0.33% | -0.36% | **+6.07%** |
| Aggregate p99 | **+5.07%** | -1.63% | -0.08% |
| Range p99 | **+6.56%** | -4.68% | -2.72% |
| Join p99 | -0.65% | +0.62% | +2.35% |
| Bundle p99 | +1.11% | +3.22% | +3.29% |
| Capacity SQL throughput | — | — | +0.27% |

The 100 ms setting fails the 5% analytical-latency preservation guard. The 200 ms setting passes medium-load screening, but fails the same guard for capacity update p99. Its lower memory does not close the PostgreSQL gap: in the medium campaign, PostgreSQL / retained / candidate charged memory is 223.80 / 491.71 / 391.54 MiB and PSS is 93.10 / 517.03 / 418.65 MiB. Point-read p99 is 0.37339 / 0.48823 / 0.47183 ms; update p99 is 1.12573 / 1.21543 / 1.21104 ms. No connection-burst campaign was run for these rejected settings.

The first 200 ms capacity campaign completes correctly but its second PostgreSQL trial records one throttled period and 582 microseconds of throttling. The entire campaign is retained and excluded from performance acceptance. A complete repeat, with the same design, has no timed CPU throttling and supplies the capacity column above. No subset of the first campaign is substituted into the repeat.

Snapshots also account for work outside the timed interval. Rust CPU including warmup and client cleanup changes +0.68%, +0.35% and -1.12% in the three valid campaigns above; including setup as well changes +0.41%, -0.26% and -1.36%. These Rust windows are unthrottled. Some PostgreSQL unpaced warmups are throttled, so the supplementary windows do not establish an all-engine warmup-inclusive CPU comparison. Every raw counter remains available in [results.json](results.json) and the archive.

All 60 trials pass result checks, exact operation counts, committed-state checks before and after restart, normal shutdown and process cleanup, with no OOM, memory-limit event or swap. This includes the 15 trials in the rejected timing campaign. The [111-member evidence archive](evidence.tar.gz) preserves 273,600 timed SQL statements, all raw reports, source hashes, launch commands, logs and the rejection record. `python3 replay.py` checks every archive hash without extracting or executing archived contents, proves that strict validation rejects the throttled campaign, validates the complete record and recomputes the summaries. Replay passes on Linux/Python 3.12 and Windows/Python 3.14.

These short WSL x86-64 screens do not establish production qualification. Charged memory and PSS use different accounting and independently sampled peaks; PostgreSQL process-sampling gaps remain recorded. All four PostgreSQL resource and latency targets remain open.

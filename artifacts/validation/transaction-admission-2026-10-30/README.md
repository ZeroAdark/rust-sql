# Transaction admission validation

This package records the three admission fixes described in [the implementation notes](../../../docs/transaction-admission.md). It is development evidence, **not production qualification**.

- Candidate: `e7d3f26e5f0acfb3de5bd9b0971a4168cbfca2d3a25407c64658721c9e4ac20a`.
- Preceding optimized fork: `26ba69060488b50cc165b9a450c397eab524c1240870012d7c775e12f4540f77`.
- Build base: `8f152bb68835ede5d4cb66c278ad30200b0a090d`, plus the four source files whose LF hashes are in [manifest.json](manifest.json). The binary banner identifies the base because the build preceded the commit.
- Archive: [evidence.tar.gz](evidence.tar.gz), 2,881,834 bytes; SHA-256 `bd38247e6c96133a8c42781d80ee6dd599f252f4471bc4d52d35a1cda60b7664`.

[checks.json](checks.json) records 52 passing admission checks, 79 session unit tests, 126 validation tests, 87 differential checks, 12 smoke checks, 510 isolation cases, ten crash/recovery cycles and logical restoration. The reviewed regression audit accepts 231/231 cases; the raw 203/231 comparison remains failed. The same admission harness fails 21 checks on the preceding binary and aborts during reload/disable after 50 checks. That negative control is intentionally incomplete and retains its exception.

[comparison.json](comparison.json) contains two separate ten-trial calibrations at 4 GiB: one with four configured active transactions, one with six. Each has five interleaved baseline/candidate pairs, six clients, 400 measured and 50 warmup bundles per client, one million fact rows, four server CPUs and separate client CPUs. All 912,000 measured statements and committed updates pass, including clean restart checks. The preceding binary bypassed extended-protocol admission at both configured limits.

With four slots, median CPU/bundle changes -0.15%, throughput -4.28%, point-read p99 +14.94% and sampled peak PSS +9.22%. This **fails** the provisional per-query latency gate. With six slots, CPU changes -0.18%, throughput +0.19%, the largest query p99 increase is +3.07%, and PSS changes -2.52%; both throughput and latency gates pass. CPU changes straddle zero across pairs and do not demonstrate a new CPU saving. No memory-limit, OOM, swap or CPU-throttling events occurred. These WSL calibrations do not replace a full memory matrix or representative Linux-server qualification.

The archive contains 1,345 hashed files: raw passing and failing admission reports, diagnostics from the intermediate build, source snapshots, build/unit logs, complete regression and isolation outputs and reviewed audit, recovery logs, both calibrations with cluster logs, and service provenance. The earlier native control's complete outputs are retained in the [validity-forwarding package](../aggregate-validity-2026-10-30/README.md); this manifest identifies that archive and includes the control report.

After extracting into a private directory, replay both calibration comparisons with Python 3 and psycopg installed:

```sh
python3 tools/analyze_calibration.py calibration/four replay-four.json source/scripts/validation
python3 tools/analyze_calibration.py calibration/six replay-six.json source/scripts/validation
cmp replay-four.json calibration/four/comparison.json
cmp replay-six.json calibration/six/comparison.json
```

All archive members were read back and hash-checked; both comparisons replayed byte-for-byte. Cluster data directories, binaries and build caches remain local and are not included.

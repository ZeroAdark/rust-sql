# Initial Linux evidence, 2026-10-29

See `docs/performance.md` and `docs/production-readiness.md` for environment, interpretation and open gates. These are development evidence, not a release certificate.

- `*-cache.csv`: seven repeated release microbenchmark runs. Each repetition includes its own CSV header; skip repeated headers when aggregating.
- `*-footprint.csv`: live requested allocator bytes for cache metadata, excluding prebuilt programs and allocator overhead.
- `unit-results.txt`: complete test-result summaries for `gravity_program` (166 passed, one ignored) and `pgsync` (17 passed, one ignored doctest).
- `server-smoke.json`: the first successful 12-check smoke run. Its temporary paths may no longer exist; later mixed reports each repeat all 12 checks with persistent work directories.
- `*-regression.out` and `*-regression.diffs.gz`: raw PostgreSQL regression summaries and compressed expectation diffs. Decompress with `gzip -dc candidate-regression.diffs.gz`.
- `*-isolation.out` and `*-isolation.diffs`: isolation evidence with the fixture's required `isolation_regression` database name.
- `mixed/*.json`: five interleaved fresh-cluster runs for each binary, including correctness checks, errors, CPU and latency metrics. `metadata.json` records binary SHA-256 values. CPU counts the full server process tree. Aggregate RSS sums processes and may double-count shared pages.

The initial exploratory threaded-client probe measured only the launcher process and was discarded from published performance results. The retained mixed reports use separate client processes and account for both launcher and server child. Source hashes are in `source-sha256.json`. The binaries were compiled before the fork commit and both display the upstream version/build ID; binary hashes distinguish them.

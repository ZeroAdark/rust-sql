# Rust SQL

An independent fork of [pgrust](https://github.com/malisper/pgrust), focused on lower CPU cost and predictable latency for mixed transactional and analytical workloads on Linux x86-64. CPU savings take priority over memory savings; bounded, measured memory increases are acceptable when they reduce CPU work.

**Status: experimental; production qualification is incomplete.** Passing targeted tests or a microbenchmark is not a durability or PostgreSQL compatibility guarantee. Use disposable workloads or replicas while the [release gates](docs/production-readiness.md) remain open.

The starting point is upstream v0.4-beta, commit `38634b408dea373f3944006dfbf74d19cbfc409f`. Gravity, the x86-64 JIT, automatic memory management, columnar MVCC, and predicate caching are already in that source. Read the [research and source map](docs/upstream-research.md) and the preserved [upstream documentation](docs/upstream-readme.md).

## Fork changes

- Expected O(1) local program-cache lookup, LRU promotion, and individual eviction, with no allocation on hits.
- Oversized entries bypass the L2 cache; replacements credit old bytes before eviction. Zero limits disable admission.
- Shared program-cache byte accounting no longer scans every entry on each insert. Global entry limits remain respected below the shard count.
- Worker wake queues use O(1) operations at both ends while preserving the existing fairness order.
- Regression tests, cache benchmarks, Linux CI, and a disposable-cluster recovery smoke test.
- Correct zero-row EXPLAIN instrumentation for direct and prepared CTAS with `WITH NO DATA`.
- Patched zstd/Wasmtime dependencies, a dependency-audit CI gate, native PostgreSQL differential checks, and whole-process crash/backup-restore probes.

The L2 admission budget counts code bytes at filing time, **not** total process memory. Prepared-plan ownership, metadata, and later JIT promotion remain separate. See [measurement limits and results](docs/performance.md).

The [compatibility investigation](docs/compatibility-investigation.md) distinguishes fixed defects, execution-engine differences, and remaining qualification work. Raw PostgreSQL regression expectations are still failing; the fork is not fully PostgreSQL-compatible or production-qualified.

## Build and validate

Install Rust through rustup (the repository pins 1.96.0), plus Linux build dependencies:

```sh
sudo apt-get install build-essential pkg-config libre2-dev clang lld libssl-dev postgresql-client python3
cargo test --locked -p gravity_program -p pgsync
cargo build --release --locked -j 4 -p main_main -p initdb
cargo run --release --locked -p gravity_program --example cache_pressure
ulimit -s 65520
python3 scripts/validation/server_smoke.py --bin-dir target/release --output artifacts/server-smoke.json
```

The smoke test creates its own local cluster, uses a private Unix socket, exercises SQL and recovery, and keeps its data/log directory for inspection. It never uses an existing database. A successful run is a small recovery check, not a crash-simulation campaign.

The SQL server and client protocol retain PostgreSQL names for compatibility. Existing commands still use `postgres`, `initdb`, and `psql`. This fork is not affiliated with the upstream team. Preserve `LICENSE`, `NOTICE`, and `ACKNOWLEDGEMENTS.md`; upstream AGPL-3.0 and component notices continue to apply.

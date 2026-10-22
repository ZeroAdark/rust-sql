# Acknowledgements

pgrust is a Rust reimplementation of [PostgreSQL](https://www.postgresql.org),
and almost all of it follows PostgreSQL's design and code. Beyond PostgreSQL,
pgrust builds on published research and on ideas from other database systems.
This file credits them. Code that pgrust ports or vendors from other projects,
and the licenses for that code, are listed in [`NOTICE`](NOTICE).

## Research

- **Self-tuning query scheduling.** Benjamin Wagner, André Kohn, Thomas
  Neumann. "Self-Tuning Query Scheduling for Analytical Workloads." SIGMOD 2021.
  Basis for the executor's scheduler: resource groups, priority decay, and
  adaptive task sizing (`crates/backend/executor/runtime`).
- **ALP.** Azim Afroozeh, Leonardo X. Kuffo, Peter Boncz. "ALP: Adaptive Lossless
  floating-Point Compression." SIGMOD 2024. Floating-point encoding
  (`crates/_support/alp`).
- **FastLanes.** Azim Afroozeh, Peter Boncz. "The FastLanes Compression Layout:
  Decoding >100 Billion Integers per Second with Scalar Code." PVLDB 16(9), 2023.
  The interleaved bit-packing layout (`crates/_support/alp/src/bitpack.rs`,
  `pgrc_codec/src/ffor.rs`).
- **BtrBlocks.** Maximilian Kuschewski, David Sauerwein, Adnan Alhomssi, Viktor
  Leis. "BtrBlocks: Efficient Columnar Compression for Data Lakes." SIGMOD 2023.
  Sample-based choice of a column encoding (`pgrc_codec/src/election.rs`).
- **OnPair.** Francesco Gargiulo, Rossano Venturini. "OnPair: Short Strings
  Compression for Fast Random Access." arXiv:2508.02280, 2025. Token training
  and the 16-byte token form for dictionary string bytes before zstd
  (`pgrc_codec/src/onpair.rs`).
- **Photon.** Alexander Behm et al. "Photon: A Fast Query Engine for Lakehouse
  Systems." SIGMOD 2022. Position-list selection vectors
  (`pgrc_batch/src/selection.rs`).
- **Unnesting.** Thomas Neumann. "Improving Unnesting of Complex Queries."
  BTW 2025. Subquery unnesting in the planner
  (`crates/backend/optimizer/plan/planner/src/unnest.rs`).
- **Groupjoin.** Guido Moerkotte, Thomas Neumann. "Accelerating Queries with
  Group-By and Join by Groupjoin." PVLDB 4(11), 2011. The groupjoin aggregation
  strategy (`crates/gravity/lowerv4/src/ops/agg/strategies/dependent.rs`).
- **Copy-and-patch compilation.** Haoran Xu, Fredrik Kjolstad. "Copy-and-Patch
  Compilation." OOPSLA 2021. JIT tuple deforming (`crates/_support/jit_deform`).
- **TPDE.** Tobias Schwarz, Tobias Kamm, Alexis Engelke. "TPDE: A Fast Adaptable
  Compiler Back-End Framework." arXiv:2505.22610, 2025. The JIT's code
  generator is a port of TPDE (`crates/tpde`; see NOTICE).
- **SSA construction.** Matthias Braun, Sebastian Buchwald, Sebastian Hack,
  Roland Leißa, Christoph Mallon, Andreas Zwinkau. "Simple and Efficient
  Construction of Static Single Assignment Form." CC 2013.
  (`crates/gravity/ir/src/promote.rs`).
- **PCT.** Sebastian Burckhardt, Pravesh Kothari, Madanlal Musuvathi, Santosh
  Nagarakatte. "A Randomized Scheduler with Probabilistic Guarantees of Finding
  Bugs." ASPLOS 2010. Randomized scheduling in the concurrency simulator
  (`crates/_support/pgsync/src/sim`).
- **Malthusian locks.** Dave Dice. "Malthusian Locks." EuroSys 2017. Lock
  hand-off policy (`crates/_support/pgsync`).
- **TinyLFU.** Gil Einziger, Roy Friedman, Ben Manes. "TinyLFU: A Highly
  Efficient Cache Admission Policy." ACM TOS 13(4), 2017; with the count-min
  sketch of Graham Cormode, S. Muthukrishnan (J. Algorithms 55(1), 2005).
  Cache admission (`crates/gravity/hosts/src/tinylfu.rs`).
- **SipHash.** Jean-Philippe Aumasson, Daniel J. Bernstein. "SipHash: a fast
  short-input PRF." INDOCRYPT 2012. 128-bit value fingerprints
  (`crates/gravity/dict`).
- **Division by constants.** Torbjörn Granlund, Peter L. Montgomery. "Division
  by Invariant Integers using Multiplication." PLDI 1994.
  (`crates/gravity/expr`).
- **HNSW.** Yu A. Malkov, D. A. Yashunin. "Efficient and Robust Approximate
  Nearest Neighbor Search Using Hierarchical Navigable Small World Graphs."
  IEEE TPAMI 42(4), 2020. The vector index, via pgvector
  (`crates/contrib/pgvector_hnsw`).
- **Dremel.** Sergey Melnik et al. "Dremel: Interactive Analysis of Web-Scale
  Datasets." PVLDB 3(1), 2010. Nested-column record assembly in the Parquet
  reader (`crates/backend/commands/copy/src/fromparquet.rs`).
- **LeanStore.** Viktor Leis, Michael Haubenschild, Alfons Kemper, Thomas
  Neumann. "LeanStore: In-Memory Data Management Beyond Main Memory." ICDE 2018.
  Cooling-stage admission for prefetched extents (`pgrc_read/src/openpart.rs`).
- **Strongly connected components.** Robert Tarjan. "Depth-First Search and
  Linear Graph Algorithms." SIAM J. Comput. 1(2), 1972.
  (`crates/gravity/backend/wasm/src/irreduce.rs`).
- Classic algorithms used throughout, largely inherited from PostgreSQL:
  Bentley and McIlroy's "Engineering a Sort Function" (Software: Practice and
  Experience, 1993), Knuth's *The Art of Computer Programming*, and Fibonacci
  hashing. Daniel Lemire's SWAR technique for parsing eight digits at once
  (`crates/backend/utils/adt/numutils`), and Wang Yi's wyhash folded-multiply
  mixing (`pgrc_write/src/dict.rs`).

## Other systems

- **PostgreSQL.** The system pgrust reimplements; see NOTICE.
- **ClickHouse.** The shapes its regular-expression
  fast paths recognize (`crates/backend/utils/adt/regexp_alt/src/program.rs`);
  128-bit fingerprints for exact distinct counting (`crates/gravity/dict`);
  per-granule size bounds and loading part metadata at open (`pgrc_format`,
  `pgrc_am`).
- **DuckDB.** Grouping-sets hash aggregation that feeds every set from one read
  of the input (`crates/gravity/decompose`); narrowing join build payloads to
  dictionary codes (`crates/backend/executor/exectuples`); the prefetch
  hole limit shared with Apache Arrow (`pgrc_read`). DuckLake's row-id packing
  (`pgrc_batch/src/rowid.rs`).
- **Apache Arrow and Apache Parquet.** The Parquet file format, which pgrust's
  reader implements from the specification (`crates/backend/access/parquet_read`),
  and Arrow's conventions for byte-level validity sweeps and prefetch
  (`pgrc_write`, `pgrc_read`).
- **Apache Iceberg.** Replace-not-stack deletion vectors (format v3)
  (`pgrc_format/src/dml.rs`).
- **Velox and Apache Spark.** LZ4-compressed spill runs
  (`crates/gravity/driver/src/mem/runcodec.rs`).
- **pgvector, pg_cron and CloudNativePG.** Ported as extensions and the
  instance agent; see NOTICE.

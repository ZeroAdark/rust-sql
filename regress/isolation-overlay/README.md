# regress/isolation-overlay

Merged onto the vendored PostgreSQL isolation tree
(`crates/postgres-18.6-reference/src/test/isolation`) to make the shadow
schedule. Copy `specs/*.spec` and `expected/*.out` in and append
`schedule.extra` to `isolation_schedule`. An `expected/*.out` here
REPLACES the vendor file of the same name. The vendor tree stays verbatim
upstream.

## Accepted divergences (expected output replaced)

- `expected/deadlock-parallel.out` (D575, Michael 2026-09-25: accept the
  divergence). Gravity's planner never builds a Gather or launches a
  PostgreSQL parallel worker (docs/gravity/milestone-1b.md §1.1 decision 6).
  So `debug_parallel_query = on` gets no single-copy Gather, and
  `e1l`/`e2l` take their xact-level advisory locks in the leader. The
  locks are then held until `e1c`/`e2c` rather than released when a
  worker exits, so `d2a1` completes after `e1c`. This is byte-identical
  to C PostgreSQL 18.6 running the spec with
  `max_parallel_workers_per_gather = 0` (its planner also disables
  parallel mode then).

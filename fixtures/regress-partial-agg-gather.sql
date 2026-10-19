-- A Gather over a partial aggregate ships per-group states, not raw rows:
-- the leader-consumption floor does not apply to it, so a high-cardinality
-- GROUP BY parallelizes as in C at the same parallel cost settings.
CREATE TABLE wio172 AS SELECT i AS id, timestamptz '2024-01-01' + (i % 100000) * interval '1s' AS tz, (i % 100000)::int8 AS i8, (i % 1000)::numeric(10,2) AS num FROM generate_series(1, 3000000) i;
ANALYZE wio172;
SET max_parallel_workers_per_gather = 4;
SET parallel_setup_cost = 100;
SET parallel_tuple_cost = 0.01;
SET min_parallel_table_scan_size = '8MB';
SET work_mem = '64MB';
SET jit = off;
EXPLAIN (COSTS OFF) SELECT tz, count(*) FROM wio172 GROUP BY 1;
EXPLAIN (COSTS OFF) SELECT i8, count(*) FROM wio172 GROUP BY 1;
EXPLAIN (COSTS OFF) SELECT num, count(*) FROM wio172 GROUP BY 1;
EXPLAIN (COSTS OFF) SELECT tz::date, count(*) FROM wio172 GROUP BY 1;
SELECT count(*) AS groups, sum(n) AS total FROM (SELECT tz, count(*) AS n FROM wio172 GROUP BY 1) s;
DROP TABLE wio172;

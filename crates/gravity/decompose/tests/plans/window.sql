-- The window family (docs/gravity/window-kernels.md §1) over scripts/window-template.sh's g / gw, planned under
-- v1's bench settings so the shapes are the page's; run by scripts/gravity-capture-plans.sh window.
\set VERBOSITY terse
\pset pager off
SET jit = off; SET enable_bitmapscan = off; SET max_parallel_workers_per_gather = 0; SET hash_mem_multiplier = 64; SET work_mem = '512MB';
SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_incremental_sort = off;
EXPLAIN (COSTS OFF) SELECT count(*) FROM (SELECT rank() OVER (PARTITION BY a ORDER BY b) AS r FROM g) s WHERE r <= 3;
EXPLAIN (COSTS OFF) SELECT count(*) FROM (SELECT b, max(b) OVER (PARTITION BY a) AS m FROM g) s WHERE b = m;
EXPLAIN (COSTS OFF) SELECT max(c) AS mx, min(c) AS mn FROM (SELECT count(*) OVER (PARTITION BY a) AS c FROM g) s;
EXPLAIN (COSTS OFF) SELECT count(*) FROM (SELECT d1, max(d1) OVER (PARTITION BY t2, c1) AS m FROM gw) s WHERE d1 = m;
EXPLAIN (COSTS OFF) SELECT rn % 7 AS k, count(*) FROM (SELECT id, row_number() OVER (ORDER BY b) AS rn FROM g) s GROUP BY rn % 7 ORDER BY k;
EXPLAIN (COSTS OFF) SELECT s % 1000 AS k, count(*) FROM (SELECT sum(a) OVER (ORDER BY b ROWS UNBOUNDED PRECEDING) AS s FROM g) t GROUP BY 1 ORDER BY 1 LIMIT 5;
-- the window columns reaching the client (no aggregate above): the shapes the decomposer serves whole
EXPLAIN (COSTS OFF) SELECT id, rank() OVER (PARTITION BY a ORDER BY b) FROM g;
EXPLAIN (COSTS OFF) SELECT b, max(b) OVER (PARTITION BY a) FROM g;
EXPLAIN (COSTS OFF) SELECT id, row_number() OVER (ORDER BY b) FROM g LIMIT 10;
EXPLAIN (COSTS OFF) SELECT id, sum(a) OVER (ORDER BY b ROWS UNBOUNDED PRECEDING), sum(a) OVER (ORDER BY b) FROM g;
EXPLAIN (COSTS OFF) SELECT id, lag(b) OVER (PARTITION BY a ORDER BY b), lead(b, 2) OVER (PARTITION BY a ORDER BY b) FROM g;
EXPLAIN (COSTS OFF) SELECT id, first_value(b) OVER (PARTITION BY a ORDER BY b RANGE BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING EXCLUDE CURRENT ROW) FROM g;
EXPLAIN (COSTS OFF) SELECT id, ntile(4) OVER (ORDER BY b), dense_rank() OVER (PARTITION BY a ORDER BY b DESC) FROM g;
EXPLAIN (COSTS OFF) SELECT count(*) OVER () FROM g;

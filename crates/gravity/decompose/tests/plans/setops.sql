-- The set-operation family (docs/gravity/setops-kernels.md §1) over scripts/setops-template.sh's g / g2 / g3,
-- planned under v1's bench settings; run by scripts/gravity-capture-plans.sh setops.
\set VERBOSITY terse
\pset pager off
SET jit = off; SET enable_bitmapscan = off; SET max_parallel_workers_per_gather = 0; SET hash_mem_multiplier = 64; SET work_mem = '512MB';
SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_incremental_sort = off;
EXPLAIN (COSTS OFF) WITH RECURSIVE t(n) AS (SELECT k FROM g2 UNION SELECT (n * 7919) % 100003 FROM t) SELECT count(*) FROM t;
EXPLAIN (COSTS OFF) WITH RECURSIVE t(k) AS (SELECT k FROM g2 UNION ALL SELECT t.k + g2.k + 1 FROM t JOIN g2 ON g2.k = t.k % 23 WHERE t.k < 100000) SELECT count(*) FROM t;
EXPLAIN (COSTS OFF) WITH RECURSIVE t(k) AS (SELECT k FROM g2 UNION ALL SELECT k + 23 FROM t WHERE k + 23 < 100000) SELECT count(*) FROM t;
EXPLAIN (COSTS OFF) WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM t WHERE n < 2000000) SELECT count(*) FROM t;
EXPLAIN (COSTS OFF) SELECT count(*) FROM (SELECT a FROM g EXCEPT SELECT k FROM g2 WHERE k < 10) s;
EXPLAIN (COSTS OFF) SELECT id FROM (SELECT id FROM g WHERE a > 20 UNION ALL SELECT k FROM g3) s ORDER BY id LIMIT 10;
EXPLAIN (COSTS OFF) SELECT count(*) FROM (SELECT id FROM g WHERE a > 15 UNION ALL SELECT k FROM g3) s;
EXPLAIN (COSTS OFF) SELECT count(*) FROM (SELECT generate_series(1, a % 3) FROM g) s;
-- the set operations themselves (no aggregate above): hashed and sorted, ALL and distinct
EXPLAIN (COSTS OFF) SELECT a FROM g EXCEPT SELECT k FROM g2 WHERE k < 10;
EXPLAIN (COSTS OFF) SELECT a FROM g INTERSECT ALL SELECT k FROM g2;
EXPLAIN (COSTS OFF) SELECT k FROM g3 EXCEPT ALL SELECT k FROM g2;
SET enable_hashagg = off;
EXPLAIN (COSTS OFF) SELECT k FROM g3 INTERSECT SELECT k FROM g2;
RESET enable_hashagg;

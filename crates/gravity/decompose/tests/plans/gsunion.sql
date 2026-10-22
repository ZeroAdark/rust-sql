-- The grouping-sets union (docs/gravity/decompose.md §2.6.1, decision 70; planner::groupingsets_union): the plan
-- shapes the planner's union-of-groupings rewrite produces, forced so every statement takes it, run over a fresh
-- initdb by scripts/gravity-capture-plans.sh gsunion; every planned statement becomes gsunion-<nnn>.plan
-- (EXPLAIN only: nothing executes but the loads). The same statements un-rewritten are in aggs / regress-full.
\set VERBOSITY terse
\pset pager off
SET client_min_messages = warning;
SET max_parallel_workers_per_gather = 0;
SET pgrust.gravity_groupingsets_rewrite = force;
CREATE TABLE g (k int NOT NULL, a int, b int, c int, t text, n numeric(12,2), f float8);
CREATE TABLE h (a int, v int);
INSERT INTO g SELECT i % 50, i % 3, i % 5, i, 's' || i, i / 7.0, i FROM generate_series(1, 2000) i;
INSERT INTO h SELECT i % 3, i FROM generate_series(1, 300) i;
ANALYZE g; ANALYZE h;

-- rollup / cube / explicit sets, GROUPING() folded per arm, a column outside the set as NULL
EXPLAIN (COSTS OFF) SELECT a, b, grouping(a, b), sum(c), count(*) FROM g GROUP BY ROLLUP (a, b);
EXPLAIN (COSTS OFF) SELECT a, b, grouping(a), grouping(b), sum(c) FROM g GROUP BY CUBE (a, b) ORDER BY 1, 2;
EXPLAIN (COSTS OFF) SELECT a, b, sum(c) FROM g GROUP BY GROUPING SETS ((a, b), (a), ()) ORDER BY 1, 2, 3;
-- the empty set alone (one arm, a plain aggregate), a duplicate set (two identical arms), GROUP BY DISTINCT
EXPLAIN (COSTS OFF) SELECT count(*), sum(c) FROM g GROUP BY GROUPING SETS (());
EXPLAIN (COSTS OFF) SELECT a, count(*) FROM g GROUP BY GROUPING SETS (a, a, ());
EXPLAIN (COSTS OFF) SELECT a, b, count(*) FROM g GROUP BY DISTINCT ROLLUP (a, b), ROLLUP (a);
-- HAVING per arm: an aggregate, a grouped column (NULL in the arms outside its set), GROUPING() in HAVING
EXPLAIN (COSTS OFF) SELECT a, b, sum(c) FROM g GROUP BY ROLLUP (a, b) HAVING sum(c) > 100 ORDER BY 1, 2;
EXPLAIN (COSTS OFF) SELECT a, b, count(*) FROM g GROUP BY GROUPING SETS ((a, b), (a)) HAVING a > 1 AND b > 1;
EXPLAIN (COSTS OFF) SELECT a, b, count(*) FROM g GROUP BY CUBE (a, b) HAVING grouping(a, b) < 3 ORDER BY 1, 2;
-- the decision-70 never-nulled prefix: a column in every set keeps its Var (no nullingrels)
EXPLAIN (COSTS OFF) SELECT a, b, sum(c) FROM g GROUP BY a, ROLLUP (b) ORDER BY 1, 2;
-- the layer above the union: DISTINCT, ORDER BY with LIMIT, a window over the union, an SRF in the arms
EXPLAIN (COSTS OFF) SELECT DISTINCT a, grouping(a) FROM g GROUP BY GROUPING SETS (a, ()) ORDER BY 1;
EXPLAIN (COSTS OFF) SELECT a, b, sum(c) FROM g GROUP BY ROLLUP (a, b) ORDER BY 3 DESC LIMIT 5;
EXPLAIN (COSTS OFF) SELECT a, sum(c), sum(sum(c)) OVER (ORDER BY a NULLS FIRST) FROM g GROUP BY GROUPING SETS ((a), ()) ORDER BY 1;
EXPLAIN (COSTS OFF) SELECT a, generate_series(1, 2) FROM g GROUP BY ROLLUP (a) ORDER BY 1, 2;
-- the input: a join, a CTE, a WHERE; a sublink in the target list; an ordered-set aggregate; a numeric / text key
EXPLAIN (COSTS OFF) SELECT g.a, h.a, sum(v) FROM g JOIN h ON h.a = g.a WHERE g.c > 10 GROUP BY ROLLUP (g.a, h.a) ORDER BY 1, 2;
EXPLAIN (COSTS OFF) WITH w AS (SELECT * FROM g WHERE c > 5) SELECT a, count(*) FROM w GROUP BY ROLLUP (a) ORDER BY 1;
EXPLAIN (COSTS OFF) SELECT a, (SELECT count(*) FROM h WHERE h.a = g.a) FROM g GROUP BY ROLLUP (a) ORDER BY 1;
EXPLAIN (COSTS OFF) SELECT a, percentile_disc(0.5) WITHIN GROUP (ORDER BY c) FROM g GROUP BY ROLLUP (a) ORDER BY 1;
EXPLAIN (COSTS OFF) SELECT n, t, count(*) FROM g GROUP BY ROLLUP (n, t) ORDER BY 1, 2;
-- a grouping-sets level below the top: a subquery and a CTE body
EXPLAIN (COSTS OFF) SELECT count(*) FROM (SELECT a, b FROM g GROUP BY CUBE (a, b)) s;
EXPLAIN (COSTS OFF) WITH r AS (SELECT a, sum(c) s FROM g GROUP BY ROLLUP (a)) SELECT * FROM r WHERE s > 10 ORDER BY 1;
-- not rewritten: a volatile function in the level (the standard plan stands under force too)
EXPLAIN (COSTS OFF) SELECT a, random() > 2 FROM g GROUP BY ROLLUP (a) ORDER BY 1;
-- not rewritten: a set-returning grouping expression (the SRF multiplies the input of every set; regress tsrf)
EXPLAIN (COSTS OFF) SELECT a, generate_series(1, 2) g, count(*) FROM g GROUP BY CUBE (a, g) ORDER BY 1, 2;

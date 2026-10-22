-- M8-RIG's m8x set (cards/M8.md L1 "tests/m8.rs"; M8-plan §8): plans the census corpora lack, run over a fresh initdb by
-- `OUT=crates/gravity/lowerv4/tests/fixtures/m8 scripts/gravity-capture-plans.sh m8x`; every planned statement becomes
-- fixtures/m8/m8x-<nnn>.plan. Two parts: G4's does-not-apply shapes (a sublink the planner pulls up into a join, an inlined
-- CTE, a flattened subquery in FROM, a hash inner: no M8 name may claim them), and statements per M8 kind (nest.loop / spool /
-- param / memo / subplan / initplan / rerun / cte) whose rows tests/m8.rs checks against PostgreSQL 18.6 (fixtures/m8/<id>.pg,
-- scripts/gravity-v4/m5_pg_golden.py) once the name is built: correlated and uncorrelated links of every SubLink type, a
-- hashed NOT IN over a NULL, a two-column ROWCOMPARE InitPlan (two setParams), lateral loops, a Material and a Memoize inner,
-- a MATERIALIZED CTE read twice, a rerun inner with ORDER BY / LIMIT, and a SubPlan past the first WHEN (a conditional site).
-- Joins are forced to nest loops (hash and merge off) where the kind is the loop itself.
\set VERBOSITY terse
\pset pager off
SET client_min_messages = warning;
SET max_parallel_workers_per_gather = 0;
CREATE TABLE tt (k int PRIMARY KEY, g int NOT NULL, t text);
INSERT INTO tt SELECT i, i % 7, (ARRAY['apple', 'banana', NULL, 'kiwi'])[1 + i % 4] FROM generate_series(1, 1000) i;
CREATE INDEX tt_g ON tt (g);
CREATE TABLE tu (k int, x int);
INSERT INTO tu SELECT i % 50, i FROM generate_series(1, 200) i;
INSERT INTO tu VALUES (NULL, 0);
ANALYZE tt;
ANALYZE tu;

-- G4: shapes that need no M8 kind or road (the pull-ups turn the sublinks into hash joins; the CTE is inlined)
SELECT count(*) FROM tt WHERE EXISTS (SELECT 1 FROM tu WHERE tu.k = tt.k);
SELECT count(*) FROM tt WHERE k IN (SELECT k FROM tu);
SELECT count(*) FROM tt WHERE NOT EXISTS (SELECT 1 FROM tu WHERE tu.k = tt.k);
WITH c AS (SELECT k, g FROM tt WHERE g = 3) SELECT count(*) FROM c;
SELECT q.g, count(*) FROM (SELECT g FROM tt WHERE k > 10) q GROUP BY q.g;
SELECT count(*) FROM tt JOIN tu ON tt.k = tu.k;

-- nest.initplan: uncorrelated links (scalar, EXISTS, a two-column ROWCOMPARE: one InitPlan, two setParams)
SELECT (SELECT max(x) FROM generate_series(1, 10) x) AS m;
SELECT x FROM generate_series(1, 5) x WHERE x > (SELECT avg(y) FROM generate_series(1, 4) y);
SELECT x FROM generate_series(1, 4) x WHERE EXISTS (SELECT 1 FROM generate_series(1, 3) y WHERE y > 2);
SELECT x FROM generate_series(1, 4) x WHERE NOT EXISTS (SELECT 1 FROM generate_series(1, 3) y WHERE y > 5);
SELECT x FROM generate_series(1, 5) x WHERE (x, x + 1) = (SELECT 3, 4);
SELECT x FROM generate_series(1, 5) x WHERE (x, 2) < (SELECT 3, 1);
SELECT (SELECT y FROM generate_series(1, 3) y WHERE y > 5) IS NULL AS empty_scalar;
SELECT x, (SELECT count(*) FROM tu) AS n FROM generate_series(1, 3) x;

-- nest.subplan: correlated links, re-run by params (scalar, EXISTS, ANY, ALL, ARRAY, a hashed NOT IN, NOT IN over a NULL)
SELECT x, (SELECT count(*) FROM generate_series(1, x) y) AS c FROM generate_series(1, 5) x;
SELECT x, (SELECT sum(y) FROM generate_series(x, 4) y) AS s FROM generate_series(1, 6) x;
SELECT x FROM generate_series(1, 6) x WHERE EXISTS (SELECT 1 FROM generate_series(1, x) y WHERE y = 3);
SELECT x FROM generate_series(1, 6) x WHERE NOT EXISTS (SELECT 1 FROM generate_series(1, x) y WHERE y = 3);
SELECT x FROM generate_series(1, 8) x WHERE x = ANY (SELECT y * 2 FROM generate_series(1, x) y);
SELECT x FROM generate_series(1, 8) x WHERE x = 1 OR x = ANY (SELECT y * 3 FROM generate_series(1, x) y);
SELECT x FROM generate_series(1, 8) x WHERE x > ALL (SELECT y * 2 FROM generate_series(1, x / 2) y);
SELECT x, ARRAY(SELECT y FROM generate_series(1, x) y ORDER BY y DESC) AS a FROM generate_series(0, 3) x;
SELECT x FROM generate_series(1, 10) x WHERE x NOT IN (SELECT y FROM generate_series(1, 5) y);
SELECT x FROM generate_series(1, 5) x WHERE x NOT IN (SELECT y FROM (VALUES (1), (NULL)) v(y));
SELECT x, x IN (SELECT y FROM (VALUES (2), (NULL)) v(y)) AS in_null FROM generate_series(1, 3) x;
SELECT k, (SELECT count(*) FROM tu WHERE tu.k = tt.k) AS c FROM tt WHERE k < 6;

-- a conditional SubPlan (past the first WHEN test, the first COALESCE argument, the first OR argument above, a FILTERed
-- aggregate's argument): run only on the rows that reach it
SELECT x, CASE WHEN x > 2 THEN (SELECT count(*) FROM generate_series(1, x) y) ELSE 0 END AS c FROM generate_series(1, 5) x;
SELECT x, COALESCE(NULLIF(x, 3), (SELECT max(y) FROM generate_series(1, x) y)) AS c FROM generate_series(1, 4) x;
SELECT count(*) FILTER (WHERE x > 2), sum((SELECT count(*) FROM generate_series(1, x) y)) FILTER (WHERE x > 3) FROM generate_series(1, 5) x;

-- nest.rerun: a subtree re-run when its param changes (ORDER BY / LIMIT inside the correlated inner)
SELECT x, (SELECT y FROM generate_series(1, 10) y WHERE y > x ORDER BY y DESC LIMIT 1) AS top FROM generate_series(1, 4) x;
SELECT x, (SELECT string_agg(y::text, ',' ORDER BY y) FROM (SELECT y FROM generate_series(1, x) y ORDER BY y DESC LIMIT 2) s) AS two FROM generate_series(1, 4) x;

-- nest.loop / nest.spool: loops over an embedded inner (INNER / LEFT / SEMI / ANTI on a non-equality qual), with and without
-- the Material spool
SET enable_hashjoin = off;
SET enable_mergejoin = off;
SELECT a, b FROM generate_series(1, 3) a JOIN generate_series(1, 4) b ON a < b;
SELECT a, b FROM generate_series(1, 4) a LEFT JOIN generate_series(1, 3) b ON a < b;
SELECT a FROM generate_series(1, 5) a WHERE EXISTS (SELECT 1 FROM generate_series(1, 3) b WHERE b > a);
SELECT a FROM generate_series(1, 5) a WHERE NOT EXISTS (SELECT 1 FROM generate_series(1, 3) b WHERE b > a);
SELECT a, b FROM (VALUES (1), (NULL), (3)) v(a) JOIN (VALUES (1), (3), (NULL)) w(b) ON a = b;
SELECT a, b FROM (VALUES (1), (2)) v(a) LEFT JOIN (SELECT b FROM generate_series(1, 0) b) w ON a = b;
SELECT a, count(tu.x) FROM generate_series(1, 4) a LEFT JOIN tu ON tu.x < a GROUP BY a;
SET enable_material = off;
SELECT a, count(tu.k) FROM generate_series(1, 4) a LEFT JOIN tu ON tu.x < a GROUP BY a;
RESET enable_material;

-- nest.param: NestLoopParam over a lateral function and over an index inner (a Memoize off, then on: nest.memo)
SELECT a, b FROM generate_series(1, 3) a, LATERAL generate_series(1, a) b;
SELECT a, b FROM generate_series(0, 3) a LEFT JOIN LATERAL generate_series(1, a) b ON true;
SET enable_memoize = off;
SELECT v.k, tt.g FROM (VALUES (1), (5), (NULL), (2000)) v(k) JOIN tt ON tt.k = v.k;
SELECT v.k, tt.g FROM (VALUES (1), (5), (NULL), (2000)) v(k) LEFT JOIN tt ON tt.k = v.k;
SELECT tu.x, tt.t FROM tu JOIN tt ON tt.k = tu.k WHERE tu.x < 12;
RESET enable_memoize;
SELECT count(*), sum(t2.g) FROM tt t1 JOIN tt t2 ON t2.k = t1.g;
SELECT t1.g, count(t2.k) FROM tt t1 LEFT JOIN tt t2 ON t2.k = t1.g + 100 GROUP BY t1.g;
RESET enable_hashjoin;
RESET enable_mergejoin;

-- nest.cte: a MATERIALIZED CTE read twice (one shared lazy spool), and one read under a nest
WITH c AS MATERIALIZED (SELECT x FROM generate_series(1, 5) x) SELECT a.x, b.x FROM c a JOIN c b ON b.x = a.x + 1;
WITH c AS MATERIALIZED (SELECT x, x * x AS sq FROM generate_series(1, 4) x) SELECT (SELECT sum(sq) FROM c), (SELECT count(*) FROM c WHERE x > 2);
WITH c AS MATERIALIZED (SELECT g, count(*) AS n FROM tt GROUP BY g) SELECT g, n FROM c WHERE n > (SELECT avg(n) FROM c);

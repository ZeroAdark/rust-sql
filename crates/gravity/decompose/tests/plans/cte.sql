-- The CTE / subquery / subplan set (docs/gravity/decompose.md §2.10: SubPlan / InitPlan, CteScan, SubqueryScan), run
-- over a fresh initdb by scripts/gravity-capture-plans.sh cte; every planned statement becomes cte-<nnn>.plan.
\set VERBOSITY terse
\pset pager off
SET client_min_messages = warning;
SET max_parallel_workers_per_gather = 0;
CREATE TABLE t (a int PRIMARY KEY, b int NOT NULL, c text, d numeric(12,2));
CREATE INDEX t_b ON t (b);
CREATE TABLE u (x int PRIMARY KEY, y int, z text);
CREATE INDEX u_y ON u (y);
CREATE TABLE tree (id int PRIMARY KEY, parent int, name text);
CREATE INDEX tree_parent ON tree (parent);
INSERT INTO t SELECT i, i % 100, 'c' || i, i * 1.5 FROM generate_series(1, 5000) i;
INSERT INTO u SELECT i, i % 50, 'z' || i FROM generate_series(1, 2000) i;
INSERT INTO tree SELECT i, CASE WHEN i = 1 THEN NULL ELSE (i / 2) END, 'n' || i FROM generate_series(1, 1000) i;
ANALYZE t; ANALYZE u; ANALYZE tree;
SET enable_bitmapscan = off;

-- InitPlans (uncorrelated subqueries evaluated once)
SELECT a FROM t WHERE b = (SELECT max(y) FROM u);
SELECT a FROM t WHERE b = (SELECT y FROM u WHERE x = 7);
SELECT a, (SELECT y FROM u WHERE x = 7) + b AS s FROM t WHERE a < 3;
SELECT a FROM t WHERE EXISTS (SELECT 1 FROM u WHERE x = 42) AND a < 3;
SELECT a FROM t WHERE a < 3 AND (SELECT count(*) FROM u) > 10;
SELECT a FROM t ORDER BY a LIMIT (SELECT min(y) FROM u) + 1;
SELECT a FROM t WHERE b = (SELECT max(y) FROM u) AND c = (SELECT z FROM u WHERE x = 3);

-- correlated SubPlans (a nested pipeline per outer row)
SELECT a, (SELECT count(*) FROM u WHERE u.y = t.b) FROM t WHERE a < 3;
SELECT a, (SELECT z FROM u WHERE u.x = t.a) FROM t WHERE a < 3;
SELECT a FROM t WHERE a < 100 AND b < (SELECT y FROM u WHERE u.x = t.a);
SELECT a, ARRAY(SELECT y FROM u WHERE u.x = t.a) FROM t WHERE a < 3;
SELECT a, EXISTS (SELECT 1 FROM u WHERE u.x = t.a) FROM t WHERE a < 3;
SELECT a, b IN (SELECT y FROM u WHERE y < 10) FROM t WHERE a < 3;
SELECT a, b = ANY (SELECT y FROM u WHERE u.x = t.a) FROM t WHERE a < 3;
SELECT a FROM t WHERE a < 3 AND b > ALL (SELECT y FROM u WHERE u.x < t.a);
SELECT a, (SELECT y FROM u WHERE u.x > t.a ORDER BY y LIMIT 1) FROM t WHERE a < 3;
SELECT a, (SELECT y FROM u WHERE u.x = t.a) FROM t WHERE a < 100 ORDER BY b LIMIT 3;

-- CTEs: materialised, referenced once and twice, from a subquery, recursive with UNION and UNION ALL, data-modifying
WITH c AS MATERIALIZED (SELECT a, b FROM t WHERE b = 1) SELECT a FROM c;
WITH c AS MATERIALIZED (SELECT a, b FROM t WHERE b = 1) SELECT c1.a FROM c c1, c c2 WHERE c1.a < c2.a;
WITH c AS MATERIALIZED (SELECT a, b FROM t WHERE b = 1) SELECT a FROM c WHERE a > 100 ORDER BY a LIMIT 5;
WITH c AS (SELECT a, b FROM t WHERE b = 1) SELECT a FROM c;
WITH c AS MATERIALIZED (SELECT a FROM t WHERE b = 1) SELECT x FROM u WHERE x IN (SELECT a FROM c);
WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM r WHERE n < 10) SELECT n FROM r;
WITH RECURSIVE r(n) AS (SELECT 1 UNION SELECT (n * 7) % 11 FROM r) SELECT n FROM r ORDER BY n;
WITH RECURSIVE sub(id, name, depth) AS (SELECT id, name, 0 FROM tree WHERE id = 1 UNION ALL SELECT tr.id, tr.name, s.depth + 1 FROM tree tr JOIN sub s ON tr.parent = s.id WHERE s.depth < 4) SELECT id, name, depth FROM sub;
WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM r WHERE n < 10) SELECT n FROM r LIMIT 3;
WITH ins AS (INSERT INTO u VALUES (100001, 1, 'cte') RETURNING x, y) SELECT x FROM ins;
WITH del AS (DELETE FROM u WHERE x = 100001 RETURNING x) SELECT x FROM del;
WITH c AS MATERIALIZED (SELECT a, b FROM t WHERE b = 1), d AS MATERIALIZED (SELECT x FROM u WHERE y = 1) SELECT a, x FROM c, d WHERE c.a = d.x;

-- SubqueryScan (a subquery the planner keeps as a scan)
SELECT s.a FROM (SELECT a FROM t ORDER BY b LIMIT 20) s WHERE s.a > 5;
SELECT s.a, s.b FROM (SELECT a, b FROM t WHERE a < 10 ORDER BY b LIMIT 5 OFFSET 1) s WHERE s.b > 1 ORDER BY s.a;
SELECT s.n FROM (SELECT a, row_number() OVER (ORDER BY b) AS n FROM t) s WHERE s.n < 4;
SELECT * FROM (SELECT a FROM t WHERE b = 1 UNION ALL SELECT x FROM u WHERE y = 1) s WHERE s.a > 10;
SELECT (SELECT a FROM t WHERE b = 1 ORDER BY a LIMIT 1) AS first, x FROM u WHERE x < 3;

-- MULTIEXPR (a per-row subquery in an UPDATE's SET list) and an UPDATE with an InitPlan
UPDATE u SET (y, z) = (SELECT b, c FROM t WHERE t.a = u.x) WHERE x = 5;
UPDATE u SET y = (SELECT max(b) FROM t) WHERE x = 5;
DELETE FROM u WHERE x = (SELECT min(a) FROM t) + 100000;

-- FACTS-1a (M8-plan §7; main 09-27): an InitPlan binds EVERY setParam (a two-column ROWCOMPARE; an uncorrelated MULTIEXPR
-- UPDATE, one InitPlan for both columns), and the conditional SubPlan site fact: a SubPlan PostgreSQL may skip for a row
-- that reaches its op (past the first WHEN test, the first COALESCE / AND / OR argument, a row comparison's first column,
-- a FILTERed aggregate's arguments, a join qual past the first) against the unconditional ones (the first WHEN test, a
-- qual conjunct: one filter op per conjunct, the SubPlan op right before its reader)
SELECT a FROM t WHERE (a, b) = (SELECT x, y FROM u WHERE x = 7);
UPDATE u SET (y, z) = (SELECT max(b), min(c) FROM t) WHERE x = 6;
SELECT a, CASE WHEN a > 1 THEN (SELECT count(*) FROM u WHERE u.y = t.b) ELSE 0 END FROM t WHERE a < 4;
SELECT a, CASE WHEN (SELECT count(*) FROM u WHERE u.y = t.b) > 30 THEN 1 ELSE 0 END FROM t WHERE a < 4;
SELECT a, COALESCE(NULLIF(a, 2), (SELECT max(x) FROM u WHERE u.y = t.b)) FROM t WHERE a < 4;
SELECT a FROM t WHERE a < 4 AND (b = 1 OR EXISTS (SELECT 1 FROM u WHERE u.y = t.b AND u.x > 1990));
SELECT a FROM t WHERE a < 4 AND ROW(a, (SELECT min(x) FROM u WHERE u.y = t.b)) < ROW(3, 5);
SELECT b, sum((SELECT count(*) FROM u WHERE u.y = t.b)) FILTER (WHERE a > 2) FROM t WHERE a < 6 GROUP BY b;
SELECT t.a, u.x FROM t LEFT JOIN u ON u.x = t.a AND u.z <> t.c AND EXISTS (SELECT 1 FROM u u2 WHERE u2.y = t.b AND u2.z = u.z) WHERE t.a < 4;
SELECT a FROM t WHERE a < 3 AND b < (SELECT y FROM u WHERE u.x = t.a) AND c <> 'x';

-- No Gather in Gravity plans (Decision 16, main 09-27): with every parallel cost at zero pgrust's planner still builds none
-- (planner lib.rs `assess_parallel = false`); tests/facts_m8.rs wraps these plans' inputs in a Gather / Gather Merge node
-- and proves decompose refuses both by kind
SET max_parallel_workers_per_gather = 2;
SET parallel_setup_cost = 0;
SET parallel_tuple_cost = 0;
SET min_parallel_table_scan_size = 0;
SELECT count(*) FROM t;
SELECT a FROM t WHERE c > 'c1' ORDER BY b LIMIT 5;

-- FACTS-1b (M8-plan §7.1; M8-FACTS): F1 rescan invariance, F2 the equality conjunct, F5 volatility / raise-free, F6 param
-- Reps, each a holds / does-not-hold pair (tests/facts_m8b.rs). Nested loops over unmaterialised inners; w has an int8
-- column (cross-type equality), a C-collated text column and a DROPPED column, z has no column at all (D870)
RESET max_parallel_workers_per_gather;
RESET parallel_setup_cost;
RESET parallel_tuple_cost;
RESET min_parallel_table_scan_size;
SET enable_hashjoin = off;
SET enable_mergejoin = off;
SET enable_material = off;
SET enable_memoize = off;
CREATE TABLE w (k int8, s text COLLATE "C", v int, gone int);
ALTER TABLE w DROP COLUMN gone;
CREATE TABLE z ();
INSERT INTO w SELECT i, 'c' || i, i % 7 FROM generate_series(1, 300) i;
INSERT INTO z SELECT FROM generate_series(1, 3);
ANALYZE w;
ANALYZE z;
CREATE SEQUENCE facts_seq;
CREATE FUNCTION facts_user(int) RETURNS int LANGUAGE sql STABLE AS 'SELECT $1 LIMIT 1';
SELECT t.a, u.x FROM t JOIN u ON u.z = t.c WHERE t.a < 3;
SELECT t.a, u.z FROM t JOIN u ON u.x = t.a WHERE t.a < 3;
SELECT t.a, u.x FROM t JOIN u ON u.z = t.c WHERE t.a < 3 AND u.y < random() * 1000;
SELECT t.a, s.r FROM t JOIN (SELECT z, random() AS r FROM u) s ON s.z = t.c WHERE t.a < 3;
SELECT t.a, u.z FROM t LEFT JOIN u ON u.z = t.c AND u.x = length(now()::text) WHERE t.a < 3;
SELECT t.a, u.z FROM t LEFT JOIN u ON u.z = t.c AND u.x = facts_user(7) WHERE t.a < 3;
WITH c AS MATERIALIZED (SELECT x, z FROM u) SELECT t.a, c.x FROM t JOIN c ON c.z = t.c WHERE t.a < 3;
WITH c AS MATERIALIZED (SELECT x, z, random() AS r FROM u) SELECT t.a, c.x FROM t JOIN c ON c.z = t.c WHERE t.a < 3;
WITH d AS (UPDATE w SET v = v WHERE k < 3 RETURNING k, s) SELECT t.a, d.s FROM t JOIN d ON d.k = t.b WHERE t.a < 3;
SELECT t.a, s.x FROM t JOIN u s TABLESAMPLE SYSTEM (50) REPEATABLE (7) ON s.z = t.c WHERE t.a < 3;
SELECT t.a FROM t CROSS JOIN z WHERE t.a < 3;
SELECT t.a, w.k FROM t JOIN w ON w.s = t.c COLLATE "C" WHERE t.a < 3;
SELECT t.a, w FROM t JOIN w ON w.k = t.b WHERE t.a < 3;
SELECT t.a, u.x FROM t JOIN u ON u.y / t.b > 0 AND u.z = t.c || '' WHERE t.a < 3;
SELECT t.a, u.x FROM t JOIN u ON u.z || '' = t.c WHERE t.a < 3;
SELECT t.a, u.x FROM t JOIN u ON u.z < t.c WHERE t.a < 3 AND u.x < 3;
SELECT a FROM t WHERE a < 3 AND b > ALL (SELECT y FROM u WHERE x < 5);
SELECT a FROM t WHERE a < 3 AND b > ALL (SELECT y + (random() * 0)::int FROM u WHERE x < 5);
SELECT a, (SELECT nextval('facts_seq') + x FROM u WHERE u.x = t.a) FROM t WHERE a < 3;
SELECT a, (SELECT count(*) FROM u WHERE u.y = t.b AND u.z < now()::text) FROM t WHERE a < 3;
SELECT a, (SELECT count(*) FROM u WHERE u.y = t.d) FROM t WHERE a < 3;
CREATE INDEX w_s ON w (s);
SELECT u.x, w.k FROM u JOIN w ON w.s = u.z COLLATE "C" WHERE u.x < 3;
-- facts-r6-3 (D1047, main 09:55Z): FACTS-1a's join qual past its first conjunct as a nested loop's join qual (the hash
-- right join form refuses by D1047's interim name: its hashed SubPlan's op and build come after the probe's pipeline).
-- `+ 0` keeps the conjuncts off a parameterized index scan's qual; the OR keeps the EXISTS a SubPlan (an inner join's
-- bare EXISTS is pulled up to a semi join).
SELECT t.a, u.x FROM t JOIN u ON u.x + 0 = t.a + 0 AND u.z <> t.c AND (t.b = 1 OR EXISTS (SELECT 1 FROM u u2 WHERE u2.y = t.b AND u2.z = u.z));

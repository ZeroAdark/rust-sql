-- The pg_regress node set (docs/gravity/decompose.md §4): one or more statements per plan node
-- kind, run over a fresh initdb by scripts/gravity-capture-plans.sh regress. Utility statements
-- are not planned (no fixture); every planned statement becomes regress-<nnn>.plan.
\set VERBOSITY terse
\pset pager off
SET client_min_messages = warning;
SET max_parallel_workers_per_gather = 0;
CREATE TABLE t (a int PRIMARY KEY, b int NOT NULL, c text, d numeric(12,2), e date);
CREATE INDEX t_b ON t (b);
CREATE INDEX t_b_c ON t (b, c);
CREATE TABLE u (x int PRIMARY KEY, y int, z text);
CREATE INDEX u_y ON u (y);
INSERT INTO t SELECT i, i % 100, 'c' || i, i * 1.5, date '2024-01-01' + (i % 365) FROM generate_series(1, 5000) i;
INSERT INTO u SELECT i, i % 50, 'z' || i FROM generate_series(1, 2000) i;
CREATE TABLE part (k int, v int) PARTITION BY RANGE (k);
CREATE TABLE part1 PARTITION OF part FOR VALUES FROM (0) TO (100);
CREATE TABLE part2 PARTITION OF part FOR VALUES FROM (100) TO (200);
CREATE INDEX part_k ON part (k);
INSERT INTO part SELECT i % 200, i FROM generate_series(1, 2000) i;
ANALYZE t; ANALYZE u; ANALYZE part;

-- Result / SeqScan / Limit / ValuesScan
SELECT 1;
SELECT a, b FROM t;
SELECT a FROM t WHERE b <> 7 AND c IS NOT NULL;
SELECT a FROM t LIMIT 3 OFFSET 2;
SELECT a FROM t WHERE a > 10 LIMIT 3;
SELECT * FROM (VALUES (1, 'x'), (2, 'y')) v(i, s) WHERE i > 0;
SELECT a FROM t WHERE false;
SELECT a FROM t ORDER BY a FETCH FIRST 2 ROWS WITH TIES;
SELECT a FROM t LIMIT ALL;

-- IndexScan / IndexOnlyScan
SET enable_seqscan = off; SET enable_bitmapscan = off;
SELECT a, b, c FROM t WHERE a = 5;
SELECT a FROM t WHERE a = 5;
SELECT b FROM t WHERE b = 3;
SELECT a, c FROM t WHERE b = 3 AND c = 'c3';
SELECT a FROM t WHERE a > 4990 ORDER BY a DESC;
SELECT b, c FROM t WHERE b = 3 AND c IS NOT NULL;
SELECT a FROM t WHERE a = ANY (ARRAY[1, 2, 3]);
RESET enable_seqscan; RESET enable_bitmapscan;

-- Bitmap family
SET enable_seqscan = off; SET enable_indexscan = off;
SELECT a FROM t WHERE b = 3;
SELECT a FROM t WHERE b = 3 OR b = 4;
SELECT a FROM t WHERE b = 3 AND a < 100;
RESET enable_seqscan; RESET enable_indexscan;

-- Tid / TidRange / Sample
SELECT a FROM t WHERE ctid = '(0,1)';
SELECT a FROM t WHERE ctid >= '(0,1)' AND ctid < '(1,0)';
SELECT a FROM t TABLESAMPLE SYSTEM (10);

-- Agg / Group / Unique / Sort / IncrementalSort
SELECT count(*) FROM t;
SELECT count(*), sum(b), avg(b), min(e), max(e) FROM t WHERE b <> 0;
SELECT b, count(*) FROM t GROUP BY b ORDER BY count(*) DESC LIMIT 5;
SELECT b, count(DISTINCT c) FROM t GROUP BY b;
SET enable_hashagg = off;
SELECT b, count(*) FROM t GROUP BY b;
SELECT b FROM t GROUP BY b;
SELECT DISTINCT b FROM t;
RESET enable_hashagg;
SELECT a FROM t ORDER BY b, c;
SELECT a FROM t ORDER BY b DESC LIMIT 10;
SET enable_seqscan = off; SET enable_incremental_sort = on;
SELECT a FROM t WHERE b > 0 ORDER BY b, a LIMIT 10;
RESET enable_seqscan;
SELECT b, c, count(*) FROM t GROUP BY GROUPING SETS ((b), (c)) ORDER BY 1, 2 LIMIT 5;

-- WindowAgg
SELECT a, row_number() OVER (PARTITION BY b ORDER BY a) FROM t;
SELECT a, sum(b) OVER (ORDER BY a ROWS BETWEEN 1 PRECEDING AND CURRENT ROW) FROM t LIMIT 5;

-- Joins: NestLoop / MergeJoin / HashJoin / Hash / Material / Memoize
SET enable_hashjoin = off; SET enable_mergejoin = off;
SELECT t.a, u.z FROM t JOIN u ON t.b = u.y WHERE t.a < 10;
SET enable_memoize = off;
SELECT t.a, u.z FROM t JOIN u ON t.b = u.y WHERE t.a < 10;
RESET enable_memoize;
SET enable_indexscan = off; SET enable_bitmapscan = off;
SELECT t.a, u.x FROM t, u WHERE t.a < u.x AND t.a < 3;
RESET enable_indexscan; RESET enable_bitmapscan;
RESET enable_hashjoin; RESET enable_mergejoin;
SET enable_nestloop = off; SET enable_mergejoin = off;
SELECT t.a, u.z FROM t JOIN u ON t.b = u.y;
SELECT t.a FROM t LEFT JOIN u ON t.a = u.x WHERE u.x IS NULL;
SELECT t.a FROM t WHERE t.b IN (SELECT y FROM u);
RESET enable_nestloop; RESET enable_mergejoin;
SET enable_nestloop = off; SET enable_hashjoin = off;
SELECT t.a, u.z FROM t JOIN u ON t.a = u.x;
RESET enable_nestloop; RESET enable_hashjoin;

-- SetOp / Append / MergeAppend / RecursiveUnion / WorkTableScan
SELECT a FROM t INTERSECT SELECT x FROM u;
SELECT a FROM t EXCEPT ALL SELECT x FROM u;
SELECT a FROM t UNION ALL SELECT x FROM u;
SELECT a FROM t UNION SELECT x FROM u;
SELECT k, v FROM part WHERE v > 10;
SET enable_seqscan = off;
SELECT k FROM part ORDER BY k LIMIT 5;
RESET enable_seqscan;
WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM r WHERE n < 10) SELECT sum(n) FROM r;

-- SubPlan / InitPlan / AlternativeSubPlan / CTE / SubqueryScan
SELECT a FROM t WHERE b = (SELECT max(y) FROM u);
SELECT a FROM t WHERE EXISTS (SELECT 1 FROM u WHERE u.y = t.b AND u.z = t.c);
SELECT a, (SELECT count(*) FROM u WHERE u.y = t.b) FROM t LIMIT 3;
WITH c AS MATERIALIZED (SELECT a, b FROM t WHERE b = 1) SELECT c1.a FROM c c1, c c2 WHERE c1.a < c2.a;
SELECT s.a FROM (SELECT a FROM t ORDER BY b LIMIT 20) s WHERE s.a > 5;

-- ProjectSet / FunctionScan / TableFuncScan
SELECT generate_series(1, 3), a FROM t LIMIT 3;
SELECT * FROM generate_series(1, 5) g(i) WHERE i > 2;
SELECT * FROM unnest(ARRAY[1, 2, 3]) WITH ORDINALITY AS o(v, n);
SELECT * FROM json_table('[{"a": 1}, {"a": 2}]', '$[*]' COLUMNS (a int PATH '$.a')) jt;
SELECT * FROM xmltable('/r/e' PASSING '<r><e>1</e></r>' COLUMNS v int PATH '.');

-- ModifyTable / LockRows / PlanRowMark / NamedTuplestoreScan
INSERT INTO u VALUES (100000, 1, 'new');
INSERT INTO u SELECT a + 200000, b, c FROM t WHERE a < 3 ON CONFLICT (x) DO UPDATE SET y = excluded.y;
UPDATE u SET y = y + 1 WHERE x = 100000 RETURNING x, y;
DELETE FROM u WHERE x = 100000;
MERGE INTO u USING (SELECT 1 AS k) s ON u.x = s.k WHEN MATCHED THEN UPDATE SET y = u.y WHEN NOT MATCHED THEN INSERT VALUES (s.k, 0, 'm');
SELECT a FROM t WHERE a = 1 FOR UPDATE;
SELECT t.a FROM t JOIN u ON t.a = u.x WHERE t.a < 3 FOR SHARE OF t SKIP LOCKED;
CREATE TABLE audit (n bigint);
CREATE FUNCTION audit_fn() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN INSERT INTO audit SELECT count(*) FROM newrows; RETURN NULL; END $$;
CREATE TRIGGER audit_t AFTER INSERT ON u REFERENCING NEW TABLE AS newrows FOR EACH STATEMENT EXECUTE FUNCTION audit_fn();
INSERT INTO u VALUES (300000, 1, 'tt');
DROP TRIGGER audit_t ON u;

-- PartitionPruneInfo (run-time pruning through a parameter) / PlanInvalItem (a user function)
PREPARE pp (int) AS SELECT v FROM part WHERE k = $1;
SET plan_cache_mode = force_generic_plan;
EXECUTE pp(5);
RESET plan_cache_mode;
CREATE FUNCTION twice(i int) RETURNS int LANGUAGE sql IMMUTABLE AS 'SELECT i * 2';
SELECT twice(a) FROM t WHERE a < 3;

-- Gather / GatherMerge (parallel plans are excluded by ruling; captured so the refusal is on record)
SET max_parallel_workers_per_gather = 2; SET parallel_setup_cost = 0; SET parallel_tuple_cost = 0; SET min_parallel_table_scan_size = 0;
SELECT count(*) FROM t WHERE b = 1;
SELECT a FROM t ORDER BY a LIMIT 5;
RESET max_parallel_workers_per_gather; RESET parallel_setup_cost; RESET parallel_tuple_cost; RESET min_parallel_table_scan_size;

-- D3 shapes (appended so the earlier fixtures keep their numbers): Memoize, the right-driven hash joins,
-- the sorted SetOp, a sort over a sequential scan, a window over a partition-sorted run with two functions
SET enable_hashjoin = off; SET enable_mergejoin = off;
SELECT t.a, u.z FROM t JOIN u ON t.b = u.y WHERE t.a < 1000;
RESET enable_hashjoin; RESET enable_mergejoin;
SET enable_nestloop = off; SET enable_mergejoin = off;
SELECT t.a, u.z FROM t RIGHT JOIN u ON t.b = u.y;
SELECT t.a, u.z FROM t FULL JOIN u ON t.a = u.x;
SELECT u.x FROM u WHERE u.y NOT IN (SELECT b FROM t WHERE b < 50);
RESET enable_nestloop; RESET enable_mergejoin;
SET enable_hashagg = off;
SELECT a FROM t INTERSECT SELECT x FROM u;
RESET enable_hashagg;
SET enable_indexscan = off; SET enable_bitmapscan = off;
SELECT a, c FROM t WHERE b < 5 ORDER BY c;
SELECT a, rank() OVER w, lag(a) OVER w, count(*) OVER (PARTITION BY b) FROM t WINDOW w AS (PARTITION BY b ORDER BY a);
RESET enable_indexscan; RESET enable_bitmapscan;

-- D4 shapes (appended): a transition-table scan without an aggregate on top (NamedTuplestoreScan served), a
-- LockRows under a Limit, a recursive CTE read directly
CREATE FUNCTION audit_rows() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN INSERT INTO audit SELECT x FROM newrows; RETURN NULL; END $$;
CREATE TRIGGER audit_r AFTER INSERT ON u REFERENCING NEW TABLE AS newrows FOR EACH STATEMENT EXECUTE FUNCTION audit_rows();
INSERT INTO u VALUES (300001, 1, 'tr');
DROP TRIGGER audit_r ON u;
SELECT a FROM t WHERE a < 10 ORDER BY a FOR UPDATE LIMIT 2;
WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM r WHERE n < 10) SELECT n FROM r;

-- D5 shapes (appended): a bitmap AND and a three-way OR, a tid point set, a BERNOULLI sample with REPEATABLE under a
-- filter, a non-count DISTINCT aggregate, the numeric register over a typmod-scaled column and a scaled product
CREATE TABLE ba (p int, q int);
CREATE INDEX ba_p ON ba (p);
CREATE INDEX ba_q ON ba (q);
INSERT INTO ba SELECT i % 100, i % 97 FROM generate_series(1, 20000) i;
ANALYZE ba;
SET enable_seqscan = off; SET enable_indexscan = off;
SELECT p, q FROM ba WHERE p = 3 AND q = 5;
SELECT p FROM ba WHERE p = 3 OR q = 5;
RESET enable_seqscan; RESET enable_indexscan;
SELECT a FROM t WHERE ctid = ANY (ARRAY['(0,1)', '(0,2)']::tid[]);
SELECT a FROM t TABLESAMPLE BERNOULLI (50) REPEATABLE (7) WHERE b = 1;
SELECT b, sum(DISTINCT a) FROM t GROUP BY b;
SELECT sum(d), avg(d), min(d), max(d), count(d) FROM t;
SELECT b, sum(d * 2), avg(d - 1.5) FROM t GROUP BY b;

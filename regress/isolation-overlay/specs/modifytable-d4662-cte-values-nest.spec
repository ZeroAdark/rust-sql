# D4662 (Delta's DVSTATS legs): a data-modifying CTE's DELETE / UPDATE joined to a VALUES list, its RETURNING read by
# the statement (READ COMMITTED). The VALUES list is a COPY mark: EvalPlanQual hands the Values Scan its stored row and
# never evaluates the list again. Three plan forms: a Nested Loop whose outer is the target's scan and whose inner is the
# Values Scan with no join qual (DVSTATS' own statement: each target row met twice), the same with the key as its Join
# Filter (session s3: Materialize and the index scans off), and a Nested Loop whose outer is the Values Scan and whose
# inner is the target's primary key index scan by the key (session s4: sequential scans off). The CTE's write waits on a
# row another session changed, then rechecks the newest version against the stored list row: a changed value passes
# (RETURNING hands the newest version on, the UPDATE computes over it), a key moved off the filter or the join key and a
# deleted row are skipped, a key moved to another value the filter still passes is written at its newest version. Each
# prints the table after. The expected output is PostgreSQL 18.6's.
setup
{
  CREATE TABLE d4662i_t (id int PRIMARY KEY, v int NOT NULL);
  INSERT INTO d4662i_t SELECT g, g * 10 FROM generate_series(1, 20) g;
  ANALYZE d4662i_t;
}
teardown { DROP TABLE d4662i_t; }

session s1
step s1v	{ BEGIN; UPDATE d4662i_t SET v = 999 WHERE id = 5; }
step s1mv	{ BEGIN; UPDATE d4662i_t SET id = 6 + 100 WHERE id = 5; }
step s1in	{ BEGIN; UPDATE d4662i_t SET id = 25 WHERE id = 5; }
step s1del	{ BEGIN; DELETE FROM d4662i_t WHERE id = 5; }
step s1c	{ COMMIT; }

session s2
step s2pl	{ EXPLAIN (COSTS OFF) WITH d AS (DELETE FROM d4662i_t USING (VALUES (1), (1)) v(x) WHERE id % 10 = 5 RETURNING id, v) SELECT count(*), sum(v) FROM d; }
step s2del	{ WITH d AS (DELETE FROM d4662i_t USING (VALUES (1), (1)) v(x) WHERE id % 10 = 5 RETURNING id, v) SELECT count(*), sum(v) FROM d; }
step s2upd	{ WITH d AS (UPDATE d4662i_t SET v = v + x FROM (VALUES (1), (2)) v(x) WHERE id % 10 = 5 RETURNING id, v, x) SELECT * FROM d ORDER BY id, x; }

session s3
setup		{ SET enable_hashjoin = off; SET enable_mergejoin = off; SET enable_material = off; SET enable_indexscan = off; SET enable_bitmapscan = off; }
step s3pl	{ EXPLAIN (COSTS OFF) WITH d AS (DELETE FROM d4662i_t USING (VALUES (5), (15)) v(x) WHERE id = v.x RETURNING id, v, x) SELECT * FROM d ORDER BY id; }
step s3del	{ WITH d AS (DELETE FROM d4662i_t USING (VALUES (5), (15)) v(x) WHERE id = v.x RETURNING id, v, x) SELECT * FROM d ORDER BY id; }

session s4
setup		{ SET enable_hashjoin = off; SET enable_mergejoin = off; SET enable_material = off; SET enable_seqscan = off; }
step s4pl	{ EXPLAIN (COSTS OFF) WITH d AS (UPDATE d4662i_t SET v = v * 100 + x FROM (VALUES (5), (15)) v(x) WHERE id = v.x RETURNING id, v) SELECT * FROM d ORDER BY id; }
step s4upd	{ WITH d AS (UPDATE d4662i_t SET v = v * 100 + x FROM (VALUES (5), (15)) v(x) WHERE id = v.x RETURNING id, v) SELECT * FROM d ORDER BY id; }

session s5
step s5show	{ SELECT id, v FROM d4662i_t WHERE id % 5 = 0 OR id > 20 ORDER BY id; }

permutation s2pl s3pl s4pl
permutation s1v s2del s1c s5show
permutation s1mv s2del s1c s5show
permutation s1in s2del s1c s5show
permutation s1del s2del s1c s5show
permutation s1v s2upd s1c s5show
permutation s1in s2upd s1c s5show
permutation s1v s3del s1c s5show
permutation s1in s3del s1c s5show
permutation s1v s4upd s1c s5show
permutation s1mv s4upd s1c s5show

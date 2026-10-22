# D4664: a data-modifying CTE's DELETE / UPDATE joined to another table that PostgreSQL plans as a Merge Join (the
# target's primary key index scan the outer, a Sort of the other table's scan the inner, that table a REFERENCE mark),
# its RETURNING read by the statement (READ COMMITTED). The CTE's write waits on a target row another session changed,
# then EvalPlanQual reruns the merge over the newest version and the other table's row refetched at its junk ctid
# (SnapshotAny: the version the join read): a changed value passes (RETURNING hands the newest version on, the UPDATE
# computes over it), a key moved off the join and a deleted row are skipped; the other table's row updated or deleted by
# the same transaction still joins, as its refetched version is the one the join read. Each prints the tables after. The
# expected output is PostgreSQL 18.6's.
setup
{
  CREATE TABLE d4664i_t (id int PRIMARY KEY, v int NOT NULL);
  INSERT INTO d4664i_t SELECT g, g * 10 FROM generate_series(1, 200) g;
  CREATE TABLE d4664i_s (k int NOT NULL, note text NOT NULL);
  INSERT INTO d4664i_s VALUES (5, 'a'), (15, 'b');
  ANALYZE d4664i_t, d4664i_s;
}
teardown { DROP TABLE d4664i_t, d4664i_s; }

session s1
step s1v	{ BEGIN; UPDATE d4664i_t SET v = 999 WHERE id = 5; }
step s1mv	{ BEGIN; UPDATE d4664i_t SET id = 205 WHERE id = 5; }
step s1del	{ BEGIN; DELETE FROM d4664i_t WHERE id = 5; }
step s1ref	{ BEGIN; UPDATE d4664i_t SET v = 999 WHERE id = 5; UPDATE d4664i_s SET k = 6, note = 'moved' WHERE k = 5; }
step s1refdel	{ BEGIN; UPDATE d4664i_t SET v = 999 WHERE id = 5; DELETE FROM d4664i_s WHERE k = 5; }
step s1c	{ COMMIT; }

session s2
step s2pl	{ EXPLAIN (COSTS OFF) WITH d AS (DELETE FROM d4664i_t USING d4664i_s WHERE id = d4664i_s.k RETURNING id, v) SELECT * FROM d ORDER BY id; }
step s2del	{ WITH d AS (DELETE FROM d4664i_t USING d4664i_s WHERE id = d4664i_s.k RETURNING id, v, note) SELECT * FROM d ORDER BY id; }
step s2upd	{ WITH u AS (UPDATE d4664i_t SET v = v + d4664i_s.k FROM d4664i_s WHERE id = d4664i_s.k RETURNING id, v, note) SELECT * FROM u ORDER BY id; }
step s2qual	{ WITH u AS (UPDATE d4664i_t SET v = v + 1 FROM d4664i_s WHERE id = d4664i_s.k AND d4664i_t.v + length(d4664i_s.note) < 500 RETURNING id, v) SELECT * FROM u ORDER BY id; }

session s3
step s3show	{ SELECT id, v FROM d4664i_t WHERE id IN (5, 15, 205) ORDER BY id; SELECT k, note FROM d4664i_s ORDER BY k; }

permutation s2pl
permutation s1v s2del s1c s3show
permutation s1mv s2del s1c s3show
permutation s1del s2del s1c s3show
permutation s1ref s2del s1c s3show
permutation s1refdel s2del s1c s3show
permutation s1v s2upd s1c s3show
permutation s1mv s2upd s1c s3show
permutation s1ref s2upd s1c s3show
permutation s1v s2qual s1c s3show

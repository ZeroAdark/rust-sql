# D4673: a data-modifying CTE's UPDATE read only by the statement's InitPlan, beside the statement's own UPDATE of
# another table (READ COMMITTED). The CTE's write waits on a row another session changed and rechecks the newest
# version (a new value passes and is counted, a key moved off the filter is skipped); the statement's write waits on
# its own row and rechecks it reading the InitPlan's value, so the CTE runs once (each matching row of t changes once).
# Each prints both tables after. The expected output is PostgreSQL 18.6's.
setup
{
  CREATE TABLE d4673i_t (id int PRIMARY KEY, v int NOT NULL);
  INSERT INTO d4673i_t SELECT g, g * 10 FROM generate_series(1, 200) g;
  CREATE TABLE d4673i_c (k int PRIMARY KEY, n int NOT NULL);
  INSERT INTO d4673i_c VALUES (1, 0), (2, 0);
  ANALYZE d4673i_t, d4673i_c;
}
teardown { DROP TABLE d4673i_t, d4673i_c; }

session s1
step s1t	{ BEGIN; UPDATE d4673i_t SET v = 999 WHERE id = 41; }
step s1tmv	{ BEGIN; UPDATE d4673i_t SET id = 1042 WHERE id = 41; }
step s1c	{ BEGIN; UPDATE d4673i_c SET n = n + 100 WHERE k = 1; }
step s1cdel	{ BEGIN; DELETE FROM d4673i_c WHERE k = 1; }
step s1com	{ COMMIT; }

session s2
step s2pl	{ EXPLAIN (COSTS OFF) WITH d AS (UPDATE d4673i_t SET v = v + 1 WHERE id % 40 = 1 RETURNING id) UPDATE d4673i_c SET n = n + (SELECT count(*) FROM d) WHERE k = 1; }
step s2upd	{ WITH d AS (UPDATE d4673i_t SET v = v + 1 WHERE id % 40 = 1 RETURNING id) UPDATE d4673i_c SET n = n + (SELECT count(*) FROM d) WHERE k = 1; }

session s3
step s3show	{ SELECT k, n FROM d4673i_c ORDER BY k; SELECT id, v FROM d4673i_t WHERE id % 40 = 1 OR id > 200 ORDER BY id; }

permutation s2pl
permutation s1t s2upd s1com s3show
permutation s1tmv s2upd s1com s3show
permutation s1c s2upd s1com s3show
permutation s1cdel s2upd s1com s3show

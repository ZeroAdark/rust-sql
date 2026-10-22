# D3668 (nestfast): an UPDATE through a WITH CHECK OPTION view whose qual is EXISTS over d68_kr, PostgreSQL hashing the
# target's own scan on the build side of a Hash Right Semi Join (d68_kr probing), the check option's SubPlan run per
# written row. Under READ COMMITTED a concurrent update of a target row makes the write wait, then recheck the row
# (EvalPlanQual): the join reruns over the newest version with d68_kr's row as the statement read it. A row whose b still
# matches is written (the check over its new version), one whose b no longer matches is skipped, a deleted row is skipped;
# a new value the check rejects raises after the wait.

setup
{
  CREATE TABLE d68_kr (a int NOT NULL);
  INSERT INTO d68_kr SELECT g FROM generate_series(1, 2000) g WHERE g % 7 <> 0;
  CREATE TABLE d68_t (id int NOT NULL, b int NOT NULL, note text NOT NULL);
  INSERT INTO d68_t SELECT g, g + 1, 'n' || g FROM generate_series(1, 9) g;
  INSERT INTO d68_t SELECT g, g % 30 + 1, 'm' || g FROM generate_series(10, 120) g;
  CREATE VIEW d68_tv AS SELECT * FROM d68_t t WHERE EXISTS (SELECT 1 FROM d68_kr r WHERE r.a = t.b) WITH CHECK OPTION;
  ANALYZE d68_kr, d68_t;
}

teardown
{
  DROP VIEW d68_tv;
  DROP TABLE d68_t, d68_kr;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1b	{ UPDATE d68_t SET b = 4, note = note || '*' WHERE id = 2; }
step s1x	{ UPDATE d68_t SET b = 7 WHERE id = 2; }
step s1d	{ DELETE FROM d68_t WHERE id = 2; }
step s1n	{ UPDATE d68_t SET note = note || '*' WHERE id = 2; }
step s1c	{ COMMIT; }

session s2
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; SET LOCAL enable_nestloop = off; SET LOCAL enable_mergejoin = off; }
step s2x	{ EXPLAIN (COSTS OFF) UPDATE d68_tv SET note = note || '+' WHERE id < 5; }
step s2u	{ UPDATE d68_tv SET note = note || '+' WHERE id < 5; }
step s2f	{ UPDATE d68_tv SET b = b + 4 WHERE id = 2; }
step s2s	{ SELECT id, b, note FROM d68_t WHERE id < 10 ORDER BY id; }
step s2c	{ COMMIT; }

permutation s2x s2c s1c
permutation s1b s2u s1c s2s s2c
permutation s1x s2u s1c s2s s2c
permutation s1d s2u s1c s2s s2c
permutation s1n s2f s1c s2c

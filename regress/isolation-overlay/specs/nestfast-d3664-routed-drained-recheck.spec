# D3664 (nestfast): an UPDATE through a WITH CHECK OPTION view over a partitioned root pruned to one leaf, PostgreSQL
# hashing the leaf's own scan on the build side of a Hash Right Semi Join (the view's EXISTS over d64_kr probing). Under
# READ COMMITTED a concurrent update of a target row makes the write wait, then recheck the row (EvalPlanQual): the join
# reruns over the newest version with d64_kr's row as the statement read it. A row whose b still matches is written (its
# new version, the check option's SubPlan over the new row); one whose b no longer matches is skipped; a deleted row is
# skipped; a row moved to another partition raises. The same for the UPDATE that moves the row to the other leaf.

setup
{
  CREATE TABLE d64_kr (a int NOT NULL);
  INSERT INTO d64_kr SELECT g FROM generate_series(1, 2000) g WHERE g % 7 <> 0;
  CREATE TABLE d64_pt (a int NOT NULL, b int NOT NULL, note text NOT NULL) PARTITION BY RANGE (a);
  CREATE TABLE d64_p1 PARTITION OF d64_pt FOR VALUES FROM (1) TO (100);
  CREATE TABLE d64_p2 PARTITION OF d64_pt FOR VALUES FROM (100) TO (200);
  INSERT INTO d64_pt SELECT g, g + 1, 'n' || g FROM generate_series(1, 9) g;
  INSERT INTO d64_pt SELECT g, g % 30 + 1, 'm' || g FROM generate_series(101, 140) g;
  CREATE VIEW d64_ptv AS SELECT * FROM d64_pt p WHERE EXISTS (SELECT 1 FROM d64_kr r WHERE r.a = p.b) WITH CHECK OPTION;
  ANALYZE d64_kr, d64_pt, d64_p1, d64_p2;
}

teardown
{
  DROP VIEW d64_ptv;
  DROP TABLE d64_pt, d64_kr;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1b	{ UPDATE d64_pt SET b = 4, note = note || '*' WHERE a = 2; }
step s1x	{ UPDATE d64_pt SET b = 7 WHERE a = 2; }
step s1m	{ UPDATE d64_pt SET a = 150 WHERE a = 2; }
step s1d	{ DELETE FROM d64_pt WHERE a = 2; }
step s1c	{ COMMIT; }

session s2
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; SET LOCAL enable_nestloop = off; SET LOCAL enable_mergejoin = off; }
step s2x	{ EXPLAIN (COSTS OFF) UPDATE d64_ptv SET note = note || '+' WHERE a < 5; }
step s2u	{ UPDATE d64_ptv SET note = note || '+' WHERE a < 5; }
step s2v	{ UPDATE d64_ptv SET a = a + 100 WHERE a < 5; }
step s2s	{ SELECT tableoid::regclass, a, b, note FROM d64_pt WHERE a < 10 OR a > 140 OR a BETWEEN 101 AND 104 ORDER BY a, note; }
step s2c	{ COMMIT; }

permutation s2x s2c s1c
permutation s1b s2u s1c s2s s2c
permutation s1x s2u s1c s2s s2c
permutation s1m s2u s1c s2s s2c
permutation s1d s2u s1c s2s s2c
permutation s1b s2v s1c s2s s2c
permutation s1x s2v s1c s2s s2c

# D3632 (nestfast): an UPDATE whose SET reads a fixed InitPlan ((SELECT 2)), a nested loop over a Materialize of a
# partitioned FROM relation's leaves pruned at run time over another ((SELECT 1)). Under READ COMMITTED a concurrent
# update of a target row makes it recheck that row (EvalPlanQual): the recheck joins the newest version again and its
# SET reads the InitPlan's value the statement read. Gravity's IR redo reads that value from the InitPlan's word. A
# changed join column skips the row; a plain and a partitioned target. D3638: an InitPlan that aggregates a table
# (max(m)) whose rows the concurrent session changes too: the recheck reads the value the statement read (5), not the
# committed one (50).

setup
{
  CREATE TABLE d3632_ab (a int NOT NULL, b int NOT NULL) PARTITION BY LIST (b);
  CREATE TABLE d3632_ab_b1 PARTITION OF d3632_ab FOR VALUES IN (1);
  CREATE TABLE d3632_ab_b2 PARTITION OF d3632_ab FOR VALUES IN (2);
  CREATE TABLE d3632_t (id int, v int);
  CREATE TABLE d3632_p (id int, v int) PARTITION BY LIST (id);
  CREATE TABLE d3632_p1 PARTITION OF d3632_p FOR VALUES IN (1, 2);
  CREATE TABLE d3632_p2 PARTITION OF d3632_p FOR VALUES IN (3, 9);
  CREATE TABLE d3632_m (m int);
  INSERT INTO d3632_ab VALUES (1, 1), (2, 1), (3, 1), (3, 2);
  INSERT INTO d3632_t VALUES (1, 10), (2, 20), (3, 30);
  INSERT INTO d3632_p VALUES (1, 10), (2, 20), (3, 30);
  INSERT INTO d3632_m VALUES (5);
}

teardown
{
  DROP TABLE d3632_ab, d3632_t, d3632_p, d3632_m;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1v	{ UPDATE d3632_t SET v = v + 100 WHERE id = 1; }
step s1k	{ UPDATE d3632_t SET id = 7 WHERE id = 2; }
step s1pv	{ UPDATE d3632_p SET v = v + 100 WHERE id = 3; }
step s1m	{ UPDATE d3632_m SET m = 50; }
step s1c	{ COMMIT; }

session s2
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; SET LOCAL enable_hashjoin = off; SET LOCAL enable_mergejoin = off; }
step s2x	{ EXPLAIN (COSTS OFF) UPDATE d3632_t SET v = v + (SELECT 2) FROM d3632_ab WHERE d3632_ab.b = (SELECT 1) AND d3632_ab.a = d3632_t.id; }
step s2u	{ UPDATE d3632_t SET v = v + (SELECT 2) FROM d3632_ab WHERE d3632_ab.b = (SELECT 1) AND d3632_ab.a = d3632_t.id RETURNING d3632_t.id, d3632_t.v; }
step s2um	{ UPDATE d3632_t SET v = v + (SELECT max(m) FROM d3632_m) FROM d3632_ab WHERE d3632_ab.b = (SELECT 1) AND d3632_ab.a = d3632_t.id RETURNING d3632_t.id, d3632_t.v; }
step s2p	{ UPDATE d3632_p SET v = v + (SELECT 2) FROM d3632_ab WHERE d3632_ab.b = (SELECT 1) AND d3632_ab.a = d3632_p.id RETURNING d3632_p.id, d3632_p.v; }
step s2s	{ SELECT * FROM d3632_t ORDER BY id; }
step s2ps	{ SELECT * FROM d3632_p ORDER BY id; }
step s2c	{ COMMIT; }

permutation s2x s2c s1c
permutation s1v s2u s1c s2s s2c
permutation s1k s2u s1c s2s s2c
permutation s1pv s2p s1c s2ps s2c
permutation s1v s1m s2um s1c s2s s2c

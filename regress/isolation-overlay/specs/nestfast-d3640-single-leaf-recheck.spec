# D3640 (nestfast): an UPDATE whose FROM relation is a partitioned table pruned to one leaf at plan time (b = 1), a
# nested loop over the target's scan and a Materialize of that leaf. Under READ COMMITTED a concurrent update of a
# target row makes it recheck that row (EvalPlanQual) against the leaf row the Material held: a new value joins again,
# a moved join column skips the row.

setup
{
  CREATE TABLE d3640_ab (a int NOT NULL, b int NOT NULL) PARTITION BY LIST (b);
  CREATE TABLE d3640_ab_b1 PARTITION OF d3640_ab FOR VALUES IN (1);
  CREATE TABLE d3640_ab_b2 PARTITION OF d3640_ab FOR VALUES IN (2);
  CREATE TABLE d3640_t (id int, v int);
  INSERT INTO d3640_ab VALUES (1, 1), (2, 1), (3, 1), (3, 2);
  INSERT INTO d3640_t VALUES (1, 10), (2, 20), (3, 30);
}

teardown
{
  DROP TABLE d3640_ab, d3640_t;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1v	{ UPDATE d3640_t SET v = v + 100 WHERE id = 1; }
step s1k	{ UPDATE d3640_t SET id = 7 WHERE id = 2; }
step s1c	{ COMMIT; }

session s2
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; SET LOCAL enable_hashjoin = off; SET LOCAL enable_mergejoin = off; }
step s2x	{ EXPLAIN (COSTS OFF) UPDATE d3640_t SET v = v + 2 FROM d3640_ab WHERE d3640_ab.b = 1 AND d3640_ab.a = d3640_t.id; }
step s2u	{ UPDATE d3640_t SET v = v + 2 FROM d3640_ab WHERE d3640_ab.b = 1 AND d3640_ab.a = d3640_t.id RETURNING d3640_t.id, d3640_t.v; }
step s2s	{ SELECT * FROM d3640_t ORDER BY id; }
step s2c	{ COMMIT; }

permutation s2x s2c s1c
permutation s1v s2u s1c s2s s2c
permutation s1k s2u s1c s2s s2c

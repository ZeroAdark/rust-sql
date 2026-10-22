# D3674 (nestfast): an UPDATE whose filter is a row NOT IN over ten columns, PostgreSQL's hashed SubPlan (a key of ten
# words, the set's wide form). Under READ COMMITTED a concurrent update of a target row makes the write wait, then
# recheck the row (EvalPlanQual): the filter runs again over the newest version, its NOT IN probing the set built once.
# A row now equal to a set row is skipped; one whose other columns changed is written; a row given a NULL key column
# answers NULL (skipped).

setup
{
  CREATE TABLE d74_a (id int, c1 int, c2 int, c3 int, c4 int, c5 int, c6 int, c7 int, c8 int, c9 int, c10 int, note text);
  INSERT INTO d74_a SELECT g, g, g, g, g, g, g, g, g, g, g, 'n' || g FROM generate_series(1, 9) g;
  CREATE TABLE d74_s (c1 int, c2 int, c3 int, c4 int, c5 int, c6 int, c7 int, c8 int, c9 int, c10 int);
  INSERT INTO d74_s SELECT g, g, g, g, g, g, g, g, g, g FROM generate_series(20, 40) g;
  INSERT INTO d74_s VALUES (5, 5, 5, 5, 5, 5, 5, 5, 5, 5);
  ANALYZE d74_a, d74_s;
}

teardown
{
  DROP TABLE d74_a, d74_s;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1k	{ UPDATE d74_a SET c1 = 30, c2 = 30, c3 = 30, c4 = 30, c5 = 30, c6 = 30, c7 = 30, c8 = 30, c9 = 30, c10 = 30 WHERE id = 2; }
step s1n	{ UPDATE d74_a SET note = note || '*' WHERE id = 2; }
step s1z	{ UPDATE d74_a SET c7 = NULL WHERE id = 2; }
step s1c	{ COMMIT; }

session s2
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s2x	{ EXPLAIN (COSTS OFF) UPDATE d74_a SET note = note || '+' WHERE id < 7 AND (c1, c2, c3, c4, c5, c6, c7, c8, c9, c10) NOT IN (SELECT * FROM d74_s); }
step s2u	{ UPDATE d74_a SET note = note || '+' WHERE id < 7 AND (c1, c2, c3, c4, c5, c6, c7, c8, c9, c10) NOT IN (SELECT * FROM d74_s); }
step s2s	{ SELECT id, c1, c7, note FROM d74_a WHERE id < 8 ORDER BY id; }
step s2c	{ COMMIT; }

permutation s2x s2c s1c
permutation s1k s2u s1c s2s s2c
permutation s1n s2u s1c s2s s2c
permutation s1z s2u s1c s2s s2c

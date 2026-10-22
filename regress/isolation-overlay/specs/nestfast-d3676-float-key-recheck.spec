# D3676 (nestfast): an UPDATE whose filter is a float8 NOT IN and a float4 NOT IN, PostgreSQL's hashed SubPlans (one
# set word each, the float's word image). Under READ COMMITTED a concurrent update of a target row makes the write wait,
# then recheck the row (EvalPlanQual): the filter runs again over the newest version, probing the sets built once. A
# row given -0 (the set holds 0) or a NaN of the other sign (the set holds NaN) is skipped; one whose other column
# changed is written.

setup
{
  CREATE TABLE d76_a (id int, f8 float8, f4 float4, note text);
  INSERT INTO d76_a SELECT g, g + 0.5, g + 0.25, 'n' || g FROM generate_series(1, 7) g;
  CREATE TABLE d76_s (f8 float8, f4 float4);
  INSERT INTO d76_s VALUES (0, 0), ('NaN', 'NaN'), (20.5, 20.25), (30.5, 30.25);
  ANALYZE d76_a, d76_s;
}

teardown
{
  DROP TABLE d76_a, d76_s;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1z	{ UPDATE d76_a SET f8 = '-0' WHERE id = 3; }
step s1n	{ UPDATE d76_a SET f8 = -'NaN'::float8 WHERE id = 3; }
step s1f	{ UPDATE d76_a SET f4 = -'NaN'::float4 WHERE id = 3; }
step s1o	{ UPDATE d76_a SET note = note || '*' WHERE id = 3; }
step s1c	{ COMMIT; }

session s2
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s2x	{ EXPLAIN (COSTS OFF) UPDATE d76_a SET note = note || '+' WHERE id < 6 AND f8 NOT IN (SELECT f8 FROM d76_s) AND f4 NOT IN (SELECT f4 FROM d76_s); }
step s2u	{ UPDATE d76_a SET note = note || '+' WHERE id < 6 AND f8 NOT IN (SELECT f8 FROM d76_s) AND f4 NOT IN (SELECT f4 FROM d76_s); }
step s2s	{ SELECT id, f8, f4, note FROM d76_a WHERE id < 7 ORDER BY id; }
step s2c	{ COMMIT; }

permutation s2x s2c s1c
permutation s1z s2u s1c s2s s2c
permutation s1n s2u s1c s2s s2c
permutation s1f s2u s1c s2s s2c
permutation s1o s2u s1c s2s s2c

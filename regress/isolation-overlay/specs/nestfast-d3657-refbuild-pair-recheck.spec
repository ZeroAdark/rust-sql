# D3657 (nestfast): lockrows REFBUILD (LockRows -> Hash Join -> (Seq Scan d57_a, Hash -> the locked table's scan), the
# probe side REFERENCE-marked) under a LIMIT, beside an InitPlan that locks a config row (the M4 pair's A). Under READ
# COMMITTED a concurrent update of a locked row makes the outer lock recheck it (EvalPlanQual): the join reruns over the
# newest version and the probe side's row refetched as the statement read it, with the InitPlan's value copied. A changed
# row comes back with its new values; one that fails the filter or whose join key moved is skipped and the next row taken.
# A concurrent update of the config row makes the InitPlan's own lock wait and recheck that row inside the InitPlan's run:
# the filter reads the newest w.

setup
{
  CREATE TABLE d57_a (id int PRIMARY KEY, owner text NOT NULL, bal int NOT NULL);
  INSERT INTO d57_a SELECT i, 'o' || (i % 7), (i * 37) % 101 FROM generate_series(1, 400) i;
  CREATE TABLE d57_t (id int PRIMARY KEY, w int NOT NULL, note text NOT NULL);
  INSERT INTO d57_t SELECT i, i % 5, 'n' || i FROM generate_series(1, 60) i;
  CREATE TABLE d57_c (name text PRIMARY KEY, w int NOT NULL);
  INSERT INTO d57_c VALUES ('q', 2);
  ANALYZE d57_a, d57_t, d57_c;
}

teardown
{
  DROP TABLE d57_a, d57_t, d57_c;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1v	{ UPDATE d57_t SET note = note || '+' WHERE id IN (7, 12); }
step s1w	{ UPDATE d57_t SET w = 3 WHERE id = 7; }
step s1k	{ UPDATE d57_t SET id = 1007 WHERE id = 7; }
step s1q	{ UPDATE d57_c SET w = 3 WHERE name = 'q'; }
step s1c	{ COMMIT; }

session s2
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; SET LOCAL enable_nestloop = off; SET LOCAL enable_mergejoin = off; }
step s2x	{ EXPLAIN (COSTS OFF) SELECT t.id, t.w, t.note FROM d57_t t JOIN d57_a a ON a.id = t.id WHERE t.w = (SELECT w FROM d57_c WHERE name = 'q' FOR UPDATE) LIMIT 3 FOR UPDATE OF t; }
step s2l	{ SELECT t.id, t.w, t.note FROM d57_t t JOIN d57_a a ON a.id = t.id WHERE t.w = (SELECT w FROM d57_c WHERE name = 'q' FOR UPDATE) LIMIT 3 FOR UPDATE OF t; }
step s2c	{ COMMIT; }

permutation s2x s2c s1c
permutation s1v s2l s1c s2c
permutation s1w s2l s1c s2c
permutation s1k s2l s1c s2c
permutation s1q s2l s1c s2c
permutation s1v s1q s2l s1c s2c

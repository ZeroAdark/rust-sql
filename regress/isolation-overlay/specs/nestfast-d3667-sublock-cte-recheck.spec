# D3667 (nestfast): lockrows SUBLOCK (a LockRows inside a correlated EXISTS SubPlan) beside a data-modifying CTE's
# INSERT that only an InitPlan of the outer query reads: WITH x AS (INSERT INTO d67_log .. RETURNING id) SELECT .. WHERE
# j.id <= (SELECT min(id) FROM x) AND EXISTS (SELECT 1 FROM d67_j k WHERE k.id = j.id AND k.state = 'ready' FOR UPDATE).
# Under READ COMMITTED a concurrent update of an inner row makes the SubPlan's lock wait, then recheck that row
# (EvalPlanQual over the inner scan alone): a row still 'ready' is locked and the outer row returned, one no longer 'ready'
# or deleted is skipped (the EXISTS is false). The CTE's INSERT runs once either way.

setup
{
  CREATE TABLE d67_j (id int PRIMARY KEY, state text NOT NULL, note text NOT NULL);
  INSERT INTO d67_j SELECT i, CASE WHEN i % 5 = 0 THEN 'done' ELSE 'ready' END, 'n' || i FROM generate_series(1, 40) i;
  CREATE TABLE d67_log (id int, note text);
  ANALYZE d67_j;
}

teardown
{
  DROP TABLE d67_j, d67_log;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1n	{ UPDATE d67_j SET note = note || '+' WHERE id = 4; }
step s1d	{ UPDATE d67_j SET state = 'done' WHERE id = 4; }
step s1r	{ UPDATE d67_j SET state = 'ready' WHERE id = 5; }
step s1x	{ DELETE FROM d67_j WHERE id = 4; }
step s1c	{ COMMIT; }

session s2
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s2x	{ EXPLAIN (COSTS OFF) WITH x AS (INSERT INTO d67_log VALUES (8, 'x') RETURNING id) SELECT j.id FROM d67_j j WHERE j.id <= (SELECT min(id) FROM x) AND EXISTS (SELECT 1 FROM d67_j k WHERE k.id = j.id AND k.state = 'ready' FOR UPDATE); }
step s2l	{ WITH x AS (INSERT INTO d67_log VALUES (8, 'x') RETURNING id) SELECT j.id FROM d67_j j WHERE j.id <= (SELECT min(id) FROM x) AND EXISTS (SELECT 1 FROM d67_j k WHERE k.id = j.id AND k.state = 'ready' FOR UPDATE) ORDER BY j.id; }
step s2s	{ SELECT id, note FROM d67_log ORDER BY id; }
step s2c	{ COMMIT; }

permutation s2x s2c s1c
permutation s1n s2l s1c s2s s2c
permutation s1d s2l s1c s2s s2c
permutation s1r s2l s1c s2s s2c
permutation s1x s2l s1c s2s s2c

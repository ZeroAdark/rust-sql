# D3660 (nestfast): lockrows SUBLOCK (a LockRows inside a correlated EXISTS SubPlan) beside an InitPlan of the outer query
# that locks a config row (the M4 pair's A). Under READ COMMITTED a concurrent update of the config row makes the
# InitPlan's own lock wait and recheck that row inside the InitPlan's run (the filter reads the newest prio); a concurrent
# update of an inner row makes the SubPlan's lock wait and recheck it (a row whose newest version fails the inner's qual
# answers EXISTS false); both together.

setup
{
  CREATE TABLE d60_j (id int PRIMARY KEY, state text NOT NULL, prio int NOT NULL);
  INSERT INTO d60_j SELECT i, CASE WHEN i % 4 = 0 THEN 'done' ELSE 'ready' END, i % 10 FROM generate_series(1, 12) i;
  CREATE TABLE d60_c (name text PRIMARY KEY, prio int NOT NULL);
  INSERT INTO d60_c VALUES ('q', 5);
  ANALYZE d60_j, d60_c;
}

teardown
{
  DROP TABLE d60_j, d60_c;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1q	{ UPDATE d60_c SET prio = 7 WHERE name = 'q'; }
step s1s	{ UPDATE d60_j SET state = 'done' WHERE id = 6; }
step s1a	{ UPDATE d60_j SET prio = prio + 0 WHERE id = 9; }
step s1c	{ COMMIT; }

session s2
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s2x	{ EXPLAIN (COSTS OFF) SELECT j.id, j.prio FROM d60_j j WHERE j.prio > (SELECT prio FROM d60_c WHERE name = 'q' FOR UPDATE) AND EXISTS (SELECT 1 FROM d60_j k WHERE k.id = j.id AND k.state = 'ready' FOR UPDATE); }
step s2l	{ SELECT j.id, j.prio FROM d60_j j WHERE j.prio > (SELECT prio FROM d60_c WHERE name = 'q' FOR UPDATE) AND EXISTS (SELECT 1 FROM d60_j k WHERE k.id = j.id AND k.state = 'ready' FOR UPDATE); }
step s2c	{ COMMIT; }

permutation s2x s2c s1c
permutation s1q s2l s1c s2c
permutation s1s s2l s1c s2c
permutation s1a s2l s1c s2c
permutation s1q s1s s2l s1c s2c

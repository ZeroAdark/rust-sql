# D3677 (nestfast): a row lock whose locking InitPlan is read only in a run-time index key expression that may raise
# (a.id <= (SELECT lim .. FOR UPDATE) - 295), the lock's rows also read by a per-row SubPlan step. Under READ COMMITTED a
# concurrent update of a locked row makes the lock wait, then recheck it (EvalPlanQual, the InitPlan's value copied):
# a row whose grp changed shows its new SubPlan value; a row moved past the key's bound is skipped. A concurrent update
# of the InitPlan's own row makes the InitPlan's lock wait and recheck: the key is computed from the newest lim.

setup
{
  CREATE TABLE d77a (id int PRIMARY KEY, grp int NOT NULL, v int NOT NULL);
  INSERT INTO d77a SELECT i, i % 7, (i * 13) % 101 FROM generate_series(1, 40) i;
  CREATE TABLE d77l (id int PRIMARY KEY, acct int NOT NULL, amt int NOT NULL);
  INSERT INTO d77l SELECT i, i % 9, (i * 7) % 61 FROM generate_series(1, 90) i;
  CREATE TABLE d77c (name text PRIMARY KEY, lim int NOT NULL);
  INSERT INTO d77c VALUES ('r', 300);
  ANALYZE d77a, d77l, d77c;
}

teardown
{
  DROP TABLE d77a, d77l, d77c;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1g	{ UPDATE d77a SET grp = 5 WHERE id = 3; }
step s1m	{ UPDATE d77a SET id = 33 WHERE id = 4; }
step s1c	{ UPDATE d77c SET lim = 298 WHERE name = 'r'; }
step s1k	{ COMMIT; }

session s2
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; SET enable_seqscan = off; SET enable_bitmapscan = off; }
step s2x	{ EXPLAIN (COSTS OFF) SELECT a.id, (SELECT max(l.amt) FROM d77l l WHERE l.acct = a.grp) AS m FROM d77a a WHERE a.id <= (SELECT lim FROM d77c WHERE name = 'r' FOR UPDATE) - 295 ORDER BY a.id FOR UPDATE; }
step s2l	{ SELECT a.id, a.grp, (SELECT max(l.amt) FROM d77l l WHERE l.acct = a.grp) AS m FROM d77a a WHERE a.id <= (SELECT lim FROM d77c WHERE name = 'r' FOR UPDATE) - 295 ORDER BY a.id FOR UPDATE; }
step s2k	{ COMMIT; }

permutation s2x s2k s1k
permutation s1g s2l s1k s2k
permutation s1m s2l s1k s2k
permutation s1c s2l s1k s2k

# D3654 / D3655 (nestfast): a row lock over its own relation over a per-row SubPlan step (AGA-35 form 7) beside an
# InitPlan that locks a config row (the M4 pair's A), read in the index key expression a.id <= LEAST((SELECT ..), 8).
# Under READ COMMITTED a concurrent update of a locked row makes the outer lock recheck it (EvalPlanQual): the recheck
# reruns the SubPlan over the newest version (a changed group moves m) and copies the InitPlan's value. A concurrent
# update of the config row makes the InitPlan's own lock wait and recheck that row inside the InitPlan's run: the bound
# is the newest lim's (5, then 7).

setup
{
  CREATE TABLE d55_a (id int PRIMARY KEY, grp int NOT NULL, v int NOT NULL);
  INSERT INTO d55_a SELECT i, i % 3, i * 10 FROM generate_series(1, 10) i;
  CREATE TABLE d55_l (id int PRIMARY KEY, acct int NOT NULL, amt int NOT NULL);
  INSERT INTO d55_l SELECT i, i % 4, i * 3 FROM generate_series(1, 40) i;
  CREATE INDEX d55_l_acct ON d55_l (acct);
  CREATE TABLE d55_c (name text PRIMARY KEY, lim int NOT NULL);
  INSERT INTO d55_c VALUES ('r', 5);
  ANALYZE d55_a, d55_l, d55_c;
}

teardown
{
  DROP TABLE d55_a, d55_l, d55_c;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1a	{ UPDATE d55_a SET v = v + 1 WHERE id = 3; }
step s1g	{ UPDATE d55_a SET grp = grp + 1 WHERE id = 2; }
step s1d	{ DELETE FROM d55_a WHERE id = 4; }
step s1r	{ UPDATE d55_c SET lim = lim + 2 WHERE name = 'r'; }
step s1c	{ COMMIT; }

session s2
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; SET LOCAL enable_seqscan = off; SET LOCAL enable_bitmapscan = off; }
step s2x	{ EXPLAIN (COSTS OFF) SELECT a.id, a.v, (SELECT max(l.amt) FROM d55_l l WHERE l.acct = a.grp) AS m FROM d55_a a WHERE a.id <= LEAST((SELECT lim FROM d55_c WHERE name = 'r' FOR UPDATE), 8) ORDER BY a.id FOR UPDATE; }
step s2l	{ SELECT a.id, a.v, (SELECT max(l.amt) FROM d55_l l WHERE l.acct = a.grp) AS m FROM d55_a a WHERE a.id <= LEAST((SELECT lim FROM d55_c WHERE name = 'r' FOR UPDATE), 8) ORDER BY a.id FOR UPDATE; }
step s2c	{ COMMIT; }

permutation s2x s2c s1c
permutation s1a s2l s1c s2c
permutation s1g s2l s1c s2c
permutation s1d s2l s1c s2c
permutation s1r s2l s1c s2c
permutation s1a s1r s2l s1c s2c

# lockrows D3287 PULLNEST: a FOR UPDATE cursor over a join gets PostgreSQL's fast-start plan, LockRows -> Nested Loop
# (Join Filter) whose outer is the locked relation's scan (pn), or the Append of a locked inheritance family (pi, rows of
# its own and two children), and whose inner is a Materialize over the other relation's filtered scan (pj). s2's FETCH
# waits on the row s1 changed and committed, then EvalPlanQual reruns the Nested Loop over the test tuples with a fresh
# Material:
#  s1b: pn's (or pi's) row 4 b moved: the row comes back, locked.
#  s1a: pn's (or pi1's) row 9 renumbered 90: the join no longer holds; the row is skipped.
#  s1w: pj's row 14 moved out of w = 4: under FOR UPDATE of both, that locked row's newest version fails the inner's
#  filter and the row is skipped; under FOR UPDATE OF i, pj is REFERENCE and the join reads the version it read.
#  s1d: pn's row 4 deleted: skipped.
# Expected output is PostgreSQL 18.6's (the C oracle).
setup
{
  CREATE TABLE pn (a int, b int NOT NULL) WITH (autovacuum_enabled = off);
  INSERT INTO pn SELECT g, g % 3 FROM generate_series(1, 15) g;
  CREATE TABLE pi (a int, b int NOT NULL) WITH (autovacuum_enabled = off);
  CREATE TABLE pi1 () INHERITS (pi) WITH (autovacuum_enabled = off);
  CREATE TABLE pi2 (c text) INHERITS (pi) WITH (autovacuum_enabled = off);
  INSERT INTO pi SELECT g, g % 3 FROM generate_series(1, 5) g;
  INSERT INTO pi1 SELECT g, g % 3 FROM generate_series(6, 10) g;
  INSERT INTO pi2 SELECT g, g % 3, 'c' || g FROM generate_series(11, 15) g;
  CREATE TABLE pj (a int PRIMARY KEY, w int NOT NULL) WITH (autovacuum_enabled = off);
  INSERT INTO pj SELECT g, g % 5 FROM generate_series(0, 39) g;
  ANALYZE pn, pi, pi1, pi2, pj;
}

teardown
{
  DROP TABLE pn, pi2, pi1, pi, pj;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1b	{ UPDATE pn SET b = b + 10 WHERE a = 4; UPDATE pi SET b = b + 10 WHERE a = 4; }
step s1a	{ UPDATE pn SET a = 90 WHERE a = 9; UPDATE pi SET a = 90 WHERE a = 9; }
step s1w	{ UPDATE pj SET w = 0 WHERE a = 14; UPDATE pn SET b = b + 10 WHERE a = 14; UPDATE pi SET b = b + 10 WHERE a = 14; }
step s1d	{ DELETE FROM pn WHERE a = 4; DELETE FROM pi WHERE a = 4; }
step s1c	{ COMMIT; }

session s2
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s2e	{ EXPLAIN (COSTS OFF) DECLARE cx CURSOR FOR SELECT i.a, j.w FROM pn i JOIN pj j ON j.a = i.a WHERE j.w = 4 FOR UPDATE; }
step s2ef	{ EXPLAIN (COSTS OFF) DECLARE cx CURSOR FOR SELECT i.a FROM pi i JOIN pj j ON j.a = i.a WHERE j.w = 4 FOR UPDATE OF i; }
step s2n	{ DECLARE cn CURSOR FOR SELECT i.a, i.b, j.w FROM pn i JOIN pj j ON j.a = i.a WHERE j.w = 4 FOR UPDATE; }
step s2f	{ DECLARE cf CURSOR FOR SELECT i.tableoid::regclass, i.a, i.b FROM pi i JOIN pj j ON j.a = i.a WHERE j.w = 4 FOR UPDATE OF i; }
step s2nf	{ FETCH ALL FROM cn; }
step s2ff	{ FETCH ALL FROM cf; }
step s2c	{ COMMIT; }

permutation s2e s2ef s1c s2c
permutation s1b s2n s2nf s1c s2c
permutation s1a s2n s2nf s1c s2c
permutation s1w s2n s2nf s1c s2c
permutation s1d s2n s2nf s1c s2c
permutation s1b s2f s2ff s1c s2c
permutation s1a s2f s2ff s1c s2c
permutation s1w s2f s2ff s1c s2c
permutation s1d s2f s2ff s1c s2c

# lockrows D3460 REFNEST-LIFE (m8_d652_lock_initplan :383 and its cursors): FOR SHARE OF r over a Nested Loop whose outer
# is r's scan, filtered by a VOLATILE InitPlan's value, and whose inner is a Materialize over the REFERENCE-marked q's
# scan (REFNEST's kept form). The Materialize's spool reuses the InitPlan Aggregate's slot number, and REFNEST took the
# Aggregate for the spool's producer (refused 'EPQ over several sites'). s1 holds r's row 2; s2 waits on it and, after s1
# commits, EvalPlanQual rechecks the newest version against r's filter with the InitPlan's value copied (never run again:
# 3 calls of rl_bump, one per rl_k row), joined to q's row refetched at its junk ctid (the version the join read):
#  s1c25: the new value kept; s1both: with q's newer row (the join reads q's old c); s1c500: dropped by the filter;
#  s1key: r's id moved, the join filter drops it; s1del: deleted; SKIP LOCKED and NOWAIT; a cursor's FETCH ALL.
# Expected output is PostgreSQL 18.6's (the C oracle).
setup
{
  CREATE SEQUENCE rl_seq;
  CREATE FUNCTION rl_bump(int) RETURNS boolean LANGUAGE plpgsql VOLATILE AS $$ BEGIN PERFORM nextval('rl_seq'); RETURN $1 > -1000; END $$;
  CREATE TABLE rl_q (id int PRIMARY KEY, c int);
  INSERT INTO rl_q SELECT i, i FROM generate_series(1, 8) i;
  CREATE TABLE rl_k (k int);
  INSERT INTO rl_k SELECT i FROM generate_series(1, 3) i;
  CREATE TABLE rl_r (id int PRIMARY KEY, c int);
  INSERT INTO rl_r SELECT i, i * 10 FROM generate_series(1, 8) i;
  ANALYZE rl_q, rl_k, rl_r;
}

teardown
{
  DROP TABLE rl_q, rl_k, rl_r;
  DROP FUNCTION rl_bump(int);
  DROP SEQUENCE rl_seq;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1c25	{ UPDATE rl_r SET c = 25 WHERE id = 2; }
step s1both	{ UPDATE rl_q SET c = 99 WHERE id = 2; UPDATE rl_r SET c = 25 WHERE id = 2; }
step s1c500	{ UPDATE rl_r SET c = 500 WHERE id = 2; }
step s1key	{ UPDATE rl_r SET id = 12 WHERE id = 2; }
step s1del	{ DELETE FROM rl_r WHERE id = 2; }
step s1c	{ COMMIT; }

session s2
setup		{ SET enable_hashjoin = off; SET enable_mergejoin = off; }
step s2e	{ EXPLAIN (COSTS OFF) SELECT q.id, q.c, r.c FROM rl_q q JOIN rl_r r ON r.id = q.id WHERE q.id <= 3 AND r.c < (SELECT max(k) * 10 + 100 FROM rl_k WHERE rl_bump(k)) FOR SHARE OF r; }
step s2l	{ SELECT q.id, q.c, r.c FROM rl_q q JOIN rl_r r ON r.id = q.id WHERE q.id <= 3 AND r.c < (SELECT max(k) * 10 + 100 FROM rl_k WHERE rl_bump(k)) FOR SHARE OF r; }
step s2k	{ SELECT q.id, q.c, r.c FROM rl_q q JOIN rl_r r ON r.id = q.id WHERE q.id <= 3 AND r.c < (SELECT max(k) * 10 + 100 FROM rl_k WHERE rl_bump(k)) FOR SHARE OF r SKIP LOCKED; }
step s2n	{ SELECT q.id, q.c, r.c FROM rl_q q JOIN rl_r r ON r.id = q.id WHERE q.id <= 3 AND r.c < (SELECT max(k) * 10 + 100 FROM rl_k WHERE rl_bump(k)) FOR SHARE OF r NOWAIT; }
step s2calls	{ SELECT last_value FROM rl_seq; }
step s2b	{ BEGIN; }
step s2dc	{ DECLARE c5 CURSOR FOR SELECT q.id, q.c, r.c FROM rl_q q JOIN rl_r r ON r.id = q.id WHERE q.id <= 3 AND r.c < (SELECT max(k) * 10 + 100 FROM rl_k WHERE rl_bump(k)) FOR SHARE OF r; }
step s2f	{ FETCH ALL FROM c5; }
step s2cc	{ COMMIT; }

permutation s2e s1c
permutation s1c25 s2l s1c s2calls
permutation s1both s2l s1c s2calls
permutation s1c500 s2l s1c s2calls
permutation s1key s2l s1c
permutation s1del s2l s1c
permutation s1c25 s2k s1c
permutation s1c25 s2n s1c
permutation s1c25 s2b s2dc s2f s1c s2cc s2calls

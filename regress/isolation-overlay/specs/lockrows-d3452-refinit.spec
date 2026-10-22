# lockrows D3452 REFINIT (m8_d652_lock_initplan m13): FOR SHARE / FOR UPDATE OF r over a hash join whose build is the
# REFERENCE-marked q's own scan, no LIMIT, beside a non-locking VOLATILE InitPlan read in r's filter. s1 holds r's row 2;
# s2 waits on it and, after s1 commits, EvalPlanQual rechecks the newest version of r's row against r's filter with the
# InitPlan's value copied (never run again: 3 calls of ri_bump, one per ri_k row), joined to q's row refetched at its junk
# ctid (the version the join read, not s1's newer one). The new value kept (s1c25, with q's newer row s1both), dropped
# by the filter (s1c500), deleted (s1del), its join key moved (s1key); SKIP LOCKED and NOWAIT.

setup
{
 CREATE SEQUENCE ri_seq;
 CREATE FUNCTION ri_bump(int) RETURNS boolean LANGUAGE plpgsql VOLATILE AS $$ BEGIN PERFORM nextval('ri_seq'); RETURN $1 > -1000; END $$;
 CREATE TABLE ri_q (id int PRIMARY KEY, c int);
 INSERT INTO ri_q SELECT i, i FROM generate_series(1, 8) i;
 CREATE TABLE ri_k (k int);
 INSERT INTO ri_k SELECT i FROM generate_series(1, 3) i;
 CREATE TABLE ri_r (id int PRIMARY KEY, c int);
 INSERT INTO ri_r SELECT i, i * 10 FROM generate_series(1, 8) i;
 ANALYZE ri_q, ri_k, ri_r;
}

teardown
{
 DROP TABLE ri_q, ri_k, ri_r;
 DROP FUNCTION ri_bump(int);
 DROP SEQUENCE ri_seq;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1c25	{ UPDATE ri_r SET c = 25 WHERE id = 2; }
step s1both	{ UPDATE ri_q SET c = 99 WHERE id = 2; UPDATE ri_r SET c = 25 WHERE id = 2; }
step s1c500	{ UPDATE ri_r SET c = 500 WHERE id = 2; }
step s1del	{ DELETE FROM ri_r WHERE id = 2; }
step s1key	{ UPDATE ri_r SET id = 12 WHERE id = 2; }
step s1c	{ COMMIT; }

session s2
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s2e	{ EXPLAIN (COSTS OFF) SELECT q.id, q.c, r.c FROM ri_q q JOIN ri_r r ON r.id = q.id WHERE q.id <= 3 AND r.c < (SELECT max(k) * 10 + 100 FROM ri_k WHERE ri_bump(k)) FOR SHARE OF r; }
step s2s	{ SELECT q.id, q.c, r.c FROM ri_q q JOIN ri_r r ON r.id = q.id WHERE q.id <= 3 AND r.c < (SELECT max(k) * 10 + 100 FROM ri_k WHERE ri_bump(k)) FOR SHARE OF r; }
step s2u	{ SELECT q.id, q.c, r.c FROM ri_q q JOIN ri_r r ON r.id = q.id WHERE q.id <= 3 AND r.c < (SELECT max(k) * 10 + 100 FROM ri_k WHERE ri_bump(k)) FOR UPDATE OF r; }
step s2k	{ SELECT q.id, q.c, r.c FROM ri_q q JOIN ri_r r ON r.id = q.id WHERE q.id <= 3 AND r.c < (SELECT max(k) * 10 + 100 FROM ri_k WHERE ri_bump(k)) FOR SHARE OF r SKIP LOCKED; }
step s2n	{ SELECT q.id, q.c, r.c FROM ri_q q JOIN ri_r r ON r.id = q.id WHERE q.id <= 3 AND r.c < (SELECT max(k) * 10 + 100 FROM ri_k WHERE ri_bump(k)) FOR SHARE OF r NOWAIT; }
step s2calls	{ SELECT last_value FROM ri_seq; }
step s2c	{ COMMIT; }

permutation s2e s1c s2c
permutation s1c25 s2s s1c s2calls s2c
permutation s1both s2u s1c s2calls s2c
permutation s1c500 s2s s1c s2calls s2c
permutation s1del s2u s1c s2c
permutation s1key s2s s1c s2c
permutation s1c25 s2k s1c s2calls s2c
permutation s1c25 s2n s1c s2c

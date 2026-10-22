# AGA-35 (lockrows 15b beside an InitPlan): a LIMIT / OFFSET above a row lock over a Sort of the locked relation's own scan
# (Limit -> LockRows -> Sort -> Seq Scan) whose filter reads an InitPlan, the job queue's pop. When s1 changed a row (READ
# COMMITTED), EvalPlanQual reruns Sort -> scan over the newest version with the InitPlan's value copied from the main run
# (EvalPlanQualStart): a row whose newest version still passes is returned with its new values; one that fails is skipped
# and the next row taken; a config row s1 changed in the same transaction does not move the copied value; SKIP LOCKED
# takes the next row past one s1 holds; NOWAIT raises 55P03; a VOLATILE InitPlan read at the first row is called once;
# one read only in an untaken CASE arm is pending at the recheck and PG forces it there (Gravity refuses that form by
# name). Which rows the statement holds (main 2026-10-06T00:02:51Z): with s2's locks held in an open transaction, s1's
# NOWAIT probe of the row after the LIMIT succeeds and of a returned row fails; under OFFSET the skipped row is locked too
# (LockRows runs under the Limit). The InitPlan read by the LIMIT / OFFSET bound only (main 2026-10-06T00:57:07Z: the
# three v4/m8_d652_lock_initplan statements form 1 serves): LIMIT (SELECT 0) reads no row, so it neither waits for s1
# nor locks; LIMIT (SELECT NULL) is no limit and locks every row it returns; OFFSET (SELECT 2) locks the two rows it
# skips, and a row whose key s1 moved out of the filter is skipped by the recheck before the OFFSET counts it.
# D3379 INITSINK: an InitPlan that locks its own row (the M4 pair: the InitPlan's LockRows and the statement's): the
# InitPlan's lock waits for s1's config row and rechecks it under its own EvalPlanQual (the new min_prio), and s2 holds
# that row after; the statement's 15b lock rechecks as before. D3379 INITCTE: a data-modifying CTE only the InitPlan reads
# beside the 15b lock: the CTE inserts once, the recheck never runs it again.
# Expected output is the C oracle's (PostgreSQL 18, pgdg).
setup
{
  CREATE TABLE ajob (id int PRIMARY KEY, prio int NOT NULL, v text NOT NULL);
  INSERT INTO ajob SELECT i, 100 - i, 'v' || i FROM generate_series(1, 24) i;
  CREATE TABLE acfg (name text PRIMARY KEY, min_prio int NOT NULL);
  INSERT INTO acfg VALUES ('q', 80);
  CREATE SEQUENCE asq;
  CREATE TABLE alog (id int NOT NULL, v text NOT NULL);
  ANALYZE ajob, acfg;
}
teardown { DROP TABLE ajob, acfg, alog; DROP SEQUENCE asq; }

session s1
setup		{ BEGIN; }
step s1v	{ UPDATE ajob SET v = v || '+' WHERE id = 20; }
step s1p	{ UPDATE ajob SET prio = 10, v = v || 'p' WHERE id = 20; }
step s1q	{ UPDATE acfg SET min_prio = 95 WHERE name = 'q'; }
step s1l	{ SELECT id FROM ajob WHERE id = 20 FOR UPDATE; }
step s1n17	{ SELECT id FROM ajob WHERE id = 17 FOR UPDATE NOWAIT; }
step s1n18	{ SELECT id FROM ajob WHERE id = 18 FOR UPDATE NOWAIT; }
step s1n20	{ SELECT id FROM ajob WHERE id = 20 FOR UPDATE NOWAIT; }
step s1v2	{ UPDATE ajob SET v = v || '+' WHERE id IN (2, 4); }
step s1k2	{ UPDATE ajob SET id = id + 100 WHERE id = 2; }
step s1n1	{ SELECT id FROM ajob WHERE id = 1 FOR UPDATE NOWAIT; }
step s1n3	{ SELECT id FROM ajob WHERE id = 3 FOR UPDATE NOWAIT; }
step s1n4	{ SELECT id FROM ajob WHERE id = 4 FOR UPDATE NOWAIT; }
step s1n6	{ SELECT id FROM ajob WHERE id = 6 FOR UPDATE NOWAIT; }
step s1nq	{ SELECT name FROM acfg WHERE name = 'q' FOR UPDATE NOWAIT; }
step s1c	{ COMMIT; }
teardown	{ ROLLBACK; }

session s2
setup		{ SET enable_indexscan = off; SET enable_bitmapscan = off; }
step s2b	{ BEGIN; }
step s2c	{ COMMIT; }
step s2x	{ EXPLAIN (COSTS OFF) SELECT id, prio, v FROM ajob WHERE prio >= (SELECT min_prio FROM acfg WHERE name = 'q') ORDER BY prio, id LIMIT 3 FOR UPDATE; }
step s2l	{ SELECT id, prio, v FROM ajob WHERE prio >= (SELECT min_prio FROM acfg WHERE name = 'q') ORDER BY prio, id LIMIT 3 FOR UPDATE; }
step s2o	{ SELECT id, prio, v FROM ajob WHERE prio >= (SELECT min_prio FROM acfg WHERE name = 'q') ORDER BY prio, id LIMIT 2 OFFSET 1 FOR NO KEY UPDATE; }
step s2sk	{ SELECT id, prio, v FROM ajob WHERE prio >= (SELECT min_prio FROM acfg WHERE name = 'q') ORDER BY prio, id LIMIT 1 FOR UPDATE SKIP LOCKED; }
step s2nw	{ SELECT id, prio, v FROM ajob WHERE prio >= (SELECT min_prio FROM acfg WHERE name = 'q') ORDER BY prio, id LIMIT 1 FOR UPDATE NOWAIT; }
step s2n	{ SELECT id, prio, v FROM ajob WHERE prio > (SELECT nextval('asq'))::int + 78 ORDER BY prio, id LIMIT 2 FOR UPDATE; }
step s2case	{ SELECT id, prio, v FROM ajob WHERE CASE WHEN prio > 0 THEN prio >= 80 ELSE prio > (SELECT nextval('asq'))::int END ORDER BY prio, id LIMIT 2 FOR UPDATE; }
step s2seq	{ SELECT last_value, is_called FROM asq; }
step s2ox	{ EXPLAIN (COSTS OFF) SELECT id, v FROM ajob WHERE id <= 5 ORDER BY id FOR UPDATE OFFSET (SELECT 2); }
step s2l0	{ SELECT id, v FROM ajob WHERE id <= 3 ORDER BY id FOR UPDATE LIMIT (SELECT 0); }
step s2ln	{ SELECT id, v FROM ajob WHERE id <= 3 ORDER BY id FOR UPDATE LIMIT (SELECT NULL::bigint); }
step s2o2	{ SELECT id, v FROM ajob WHERE id <= 5 ORDER BY id FOR UPDATE OFFSET (SELECT 2); }
step s2ix	{ EXPLAIN (COSTS OFF) SELECT id, prio, v FROM ajob WHERE prio >= (SELECT min_prio FROM acfg WHERE name = 'q' FOR UPDATE) ORDER BY prio, id LIMIT 2 FOR UPDATE; }
step s2il	{ SELECT id, prio, v FROM ajob WHERE prio >= (SELECT min_prio FROM acfg WHERE name = 'q' FOR UPDATE) ORDER BY prio, id LIMIT 2 FOR UPDATE; }
step s2cx	{ EXPLAIN (COSTS OFF) WITH w AS (INSERT INTO alog VALUES (1, 'x') RETURNING id) SELECT id, prio, v FROM ajob WHERE prio >= (SELECT min(id) FROM w) + 79 ORDER BY prio, id LIMIT 2 FOR UPDATE; }
step s2ic	{ WITH w AS (INSERT INTO alog VALUES (1, 'x') RETURNING id) SELECT id, prio, v FROM ajob WHERE prio >= (SELECT min(id) FROM w) + 79 ORDER BY prio, id LIMIT 2 FOR UPDATE; }
step s2log	{ SELECT count(*), min(id) FROM alog; }

# prio >= 80 by prio: 20 (80), 19 (81), 18 (82), 17 (83), ...
permutation s2x s1v s2l s1c
permutation s1p s2l s1c
permutation s1q s1v s2l s1c
permutation s1v s2o s1c
permutation s1p s2o s1c
permutation s1l s2sk s1c
permutation s1l s2nw s1c
permutation s1v s2n s1c s2seq
permutation s1v s2case s1c s2seq
# s2 holds its locks: LIMIT 3 locks 20 19 18 (17, the 4th, is free); LIMIT 2 OFFSET 1 locks 20 (the OFFSET row) 19 18
permutation s2b s2l s1n17 s1n18 s2c s1c
permutation s2b s2o s1n17 s1n20 s2c s1c
# the InitPlan read by the bound only: LIMIT (SELECT NULL) returns 1 2 3 (2 with s1's value); OFFSET (SELECT 2) after
# s1 moved 2 out: 1 3 4 5 pass, 4 5 returned; LIMIT (SELECT 0) returns no row without waiting for s1
permutation s1v2 s2ln s1c
permutation s2ox s1k2 s2o2 s1c
permutation s1k2 s2l0 s1c
# s2 holds its locks: LIMIT (SELECT 0) locks nothing; LIMIT (SELECT NULL) locks 1 2 3 (4 is free); OFFSET (SELECT 2)
# locks 1 2 (the OFFSET rows) 3 4 5 (6 is free)
permutation s2b s2l0 s1n1 s2c s1c
permutation s2b s2ln s1n4 s1n3 s2c s1c
permutation s2b s2o2 s1n6 s1n1 s2c s1c
# D3379 INITSINK: s1 moved the config row's min_prio to 95: the InitPlan's lock waits, rechecks it and reads 95 (5 4); s1
# changed or moved row 20: the InitPlan's value stays 80 and the statement's lock rechecks 20 (20 19, or 19 18); with s2's
# locks held, the config row is s2's (s1's NOWAIT raises 55P03)
permutation s2ix s1q s2il s1c
permutation s1v s2il s1c
permutation s1p s2il s1c
permutation s2b s2il s1nq s2c s1c
# D3379 INITCTE: the CTE inserts one row; the recheck of row 20 (changed or moved) never inserts again
permutation s2cx s1v s2ic s1c s2log
permutation s1p s2ic s1c s2log

# AGA-35 form 5 (lockrows REFNEST beside an InitPlan): a row lock over an INNER nested loop whose outer is the locked
# relation's own scan and whose inner is the REFERENCE-marked relation's scan (Limit -> LockRows -> Nested Loop -> Index
# Scan j, Index Scan w under the nest's parameter; or LockRows -> Nested Loop -> Seq Scan j, Materialize -> Seq Scan w),
# FOR UPDATE OF j, its quals reading an InitPlan. When s1 changed a locked row (READ COMMITTED), EvalPlanQual reruns the
# join over the newest version of the locked row and the REFERENCE row refetched at its ctid (the version the main run
# joined, EvalPlanQualFetchRowMark), rechecked under the nest's parameters from the newest version, with the InitPlan's
# value copied from the main run (EvalPlanQualStart): a row whose newest version still passes is returned with its new
# values; one that fails the filter, or whose join key moved off the refetched row, is skipped and the next row taken; a
# config row s1 changed does not move the copied value; a REFERENCE row s1 changed is read as the main run saw it, in the
# outer's filter and in its own; SKIP LOCKED takes the next row past one s1 holds; NOWAIT raises 55P03; a VOLATILE
# InitPlan read at the first row is called once; one read only in an untaken CASE arm is pending at the recheck and PG
# forces it there (Gravity refuses that form by name). Which rows the statement holds: with s2's locks held in an open
# transaction, s1's NOWAIT probe of the row after the LIMIT succeeds, of a returned row fails, and of the REFERENCE row
# it joined succeeds (a REFERENCE mark locks nothing); under OFFSET the skipped row is locked too (LockRows runs under the
# Limit). s3 takes the Materialize inner (no LIMIT). Expected output is the C oracle's (PostgreSQL 18, pgdg).
setup
{
  CREATE TABLE njob (id int PRIMARY KEY, wid int NOT NULL, prio int NOT NULL, v text NOT NULL);
  INSERT INTO njob SELECT i, 1 + i % 100, i, 'v' || i FROM generate_series(1, 2000) i;
  CREATE TABLE nwrk (id int PRIMARY KEY, name text NOT NULL, cap int NOT NULL);
  INSERT INTO nwrk SELECT w, 'w' || w, w * 10 FROM generate_series(1, 100) w;
  CREATE TABLE ncfg (name text PRIMARY KEY, min_prio int NOT NULL, min_cap int NOT NULL);
  INSERT INTO ncfg VALUES ('q', 5, 20);
  CREATE SEQUENCE nsq;
  CREATE TABLE nlog (id int NOT NULL, v text NOT NULL);
  ANALYZE njob, nwrk, ncfg;
}
teardown { DROP TABLE njob, nwrk, ncfg, nlog; DROP SEQUENCE nsq; }

session s1
setup		{ BEGIN; }
step s1v	{ UPDATE njob SET v = v || '+' WHERE id = 5; }
step s1p	{ UPDATE njob SET prio = 1, v = v || 'p' WHERE id = 5; }
step s1j	{ UPDATE njob SET wid = 7, v = v || 'j' WHERE id = 5; }
step s1q	{ UPDATE ncfg SET min_prio = 7, min_cap = 40 WHERE name = 'q'; }
step s1g	{ UPDATE nwrk SET cap = 0, name = name || '+' WHERE id = 6; }
step s1l	{ SELECT id FROM njob WHERE id = 5 FOR UPDATE; }
step s1n5	{ SELECT id FROM njob WHERE id = 5 FOR UPDATE NOWAIT; }
step s1n7	{ SELECT id FROM njob WHERE id = 7 FOR UPDATE NOWAIT; }
step s1n8	{ SELECT id FROM njob WHERE id = 8 FOR UPDATE NOWAIT; }
step s1w6	{ SELECT id FROM nwrk WHERE id = 6 FOR UPDATE NOWAIT; }
step s1v1	{ UPDATE njob SET v = v || '+' WHERE id = 101; }
step s1j1	{ UPDATE njob SET wid = 3, v = v || 'j' WHERE id = 101; }
step s1g2	{ UPDATE nwrk SET cap = 0, name = name || '+' WHERE id = 2; }
step s1nq	{ SELECT name FROM ncfg WHERE name = 'q' FOR UPDATE NOWAIT; }
step s1c	{ COMMIT; }
teardown	{ ROLLBACK; }

session s2
setup		{ SET enable_seqscan = off; SET enable_bitmapscan = off; SET enable_hashjoin = off; SET enable_mergejoin = off; }
step s2b	{ BEGIN; }
step s2c	{ COMMIT; }
step s2x	{ EXPLAIN (COSTS OFF) SELECT j.id, j.v, w.name FROM njob j JOIN nwrk w ON w.id = j.wid WHERE j.prio >= (SELECT min_prio FROM ncfg WHERE name = 'q') ORDER BY j.id LIMIT 3 FOR UPDATE OF j; }
step s2l	{ SELECT j.id, j.v, w.name FROM njob j JOIN nwrk w ON w.id = j.wid WHERE j.prio >= (SELECT min_prio FROM ncfg WHERE name = 'q') ORDER BY j.id LIMIT 3 FOR UPDATE OF j; }
step s2rx	{ EXPLAIN (COSTS OFF) SELECT j.id, j.v, w.cap FROM njob j JOIN nwrk w ON w.id = j.wid WHERE j.prio >= 5 AND w.cap >= (SELECT min_cap FROM ncfg WHERE name = 'q') ORDER BY j.id LIMIT 3 FOR UPDATE OF j; }
step s2r	{ SELECT j.id, j.v, w.cap FROM njob j JOIN nwrk w ON w.id = j.wid WHERE j.prio >= 5 AND w.cap >= (SELECT min_cap FROM ncfg WHERE name = 'q') ORDER BY j.id LIMIT 3 FOR UPDATE OF j; }
step s2o	{ SELECT j.id, j.v, w.name FROM njob j JOIN nwrk w ON w.id = j.wid WHERE j.prio >= (SELECT min_prio FROM ncfg WHERE name = 'q') ORDER BY j.id LIMIT 2 OFFSET 1 FOR NO KEY UPDATE OF j; }
step s2sk	{ SELECT j.id, j.v, w.name FROM njob j JOIN nwrk w ON w.id = j.wid WHERE j.prio >= (SELECT min_prio FROM ncfg WHERE name = 'q') ORDER BY j.id LIMIT 1 FOR UPDATE OF j SKIP LOCKED; }
step s2nw	{ SELECT j.id, j.v, w.name FROM njob j JOIN nwrk w ON w.id = j.wid WHERE j.prio >= (SELECT min_prio FROM ncfg WHERE name = 'q') ORDER BY j.id LIMIT 1 FOR UPDATE OF j NOWAIT; }
step s2n	{ SELECT j.id, j.v, w.name FROM njob j JOIN nwrk w ON w.id = j.wid WHERE j.prio > (SELECT nextval('nsq'))::int + 3 ORDER BY j.id LIMIT 2 FOR UPDATE OF j; }
step s2case	{ SELECT j.id, j.v, w.name FROM njob j JOIN nwrk w ON w.id = j.wid WHERE CASE WHEN j.prio > 0 THEN j.prio >= 5 ELSE j.prio > (SELECT nextval('nsq'))::int END ORDER BY j.id LIMIT 2 FOR UPDATE OF j; }
step s2seq	{ SELECT last_value, is_called FROM nsq; }
step s2ix	{ EXPLAIN (COSTS OFF) SELECT j.id, j.v, w.name FROM njob j JOIN nwrk w ON w.id = j.wid WHERE j.prio >= (SELECT min_prio FROM ncfg WHERE name = 'q' FOR UPDATE) ORDER BY j.id LIMIT 2 FOR UPDATE OF j; }
step s2il	{ SELECT j.id, j.v, w.name FROM njob j JOIN nwrk w ON w.id = j.wid WHERE j.prio >= (SELECT min_prio FROM ncfg WHERE name = 'q' FOR UPDATE) ORDER BY j.id LIMIT 2 FOR UPDATE OF j; }
step s2cx	{ EXPLAIN (COSTS OFF) WITH x AS (INSERT INTO nlog VALUES (1, 'x') RETURNING id) SELECT j.id, j.v, w.name FROM njob j JOIN nwrk w ON w.id = j.wid WHERE j.prio >= (SELECT min(id) FROM x) + 4 ORDER BY j.id LIMIT 2 FOR UPDATE OF j; }
step s2ic	{ WITH x AS (INSERT INTO nlog VALUES (1, 'x') RETURNING id) SELECT j.id, j.v, w.name FROM njob j JOIN nwrk w ON w.id = j.wid WHERE j.prio >= (SELECT min(id) FROM x) + 4 ORDER BY j.id LIMIT 2 FOR UPDATE OF j; }
step s2log	{ SELECT count(*), min(id) FROM nlog; }

session s3
setup		{ SET enable_indexscan = off; SET enable_bitmapscan = off; SET enable_hashjoin = off; SET enable_mergejoin = off; SET enable_memoize = off; }
step s3x	{ EXPLAIN (COSTS OFF) SELECT j.id, j.v, w.name FROM njob j JOIN nwrk w ON w.id = j.wid WHERE j.prio >= (SELECT min_prio FROM ncfg WHERE name = 'q') AND j.id <= 300 AND w.id <= 4 FOR UPDATE OF j; }
step s3m	{ SELECT j.id, j.v, w.name FROM njob j JOIN nwrk w ON w.id = j.wid WHERE j.prio >= (SELECT min_prio FROM ncfg WHERE name = 'q') AND j.id <= 300 AND w.id <= 4 FOR UPDATE OF j; }
step s3rx	{ EXPLAIN (COSTS OFF) SELECT j.id, j.v, w.cap FROM njob j JOIN nwrk w ON w.id = j.wid WHERE j.id <= 110 AND w.id <= 12 AND w.cap >= (SELECT min_cap FROM ncfg WHERE name = 'q') FOR UPDATE OF j; }
step s3r	{ SELECT j.id, j.v, w.cap FROM njob j JOIN nwrk w ON w.id = j.wid WHERE j.id <= 110 AND w.id <= 12 AND w.cap >= (SELECT min_cap FROM ncfg WHERE name = 'q') FOR UPDATE OF j; }

# prio >= 5 by id: 5 (w6), 6 (w7), 7 (w8), 8 (w9), ...
permutation s2x s1v s2l s1c
permutation s1p s2l s1c
permutation s1j s2l s1c
permutation s1q s1v s2l s1c
permutation s1g s1v s2l s1c
permutation s2rx s1g s1v s2r s1c
permutation s1q s1v s2r s1c
permutation s1v s2o s1c
permutation s1p s2o s1c
permutation s1l s2sk s1c
permutation s1l s2nw s1c
permutation s1v s2n s1c s2seq
permutation s1v s2case s1c s2seq
# s2 holds its locks: LIMIT 3 locks 5 6 7 (8, the 4th, is free; nwrk 6, row 5's REFERENCE row, is not locked);
# LIMIT 2 OFFSET 1 locks 5 (the OFFSET row) 6 7
permutation s2b s2l s1n8 s1w6 s1n7 s2c s1c
permutation s2b s2o s1n8 s1n5 s2c s1c
# the Materialize inner (w spooled, j the outer): the j rows of w1 .. w4 (s3m: 100 .. 103, 200 .. 203, 300; s3r, w2 .. w12 under
# j.id <= 110 and w.id <= 12: 1 .. 11, 101 .. 110); s1 changes row 101 (w2) and w2
permutation s3x s1v1 s3m s1c
permutation s1j1 s3m s1c
permutation s1g2 s1v1 s3m s1c
permutation s3rx s1g2 s1v1 s3r s1c
permutation s1q s1v1 s3r s1c
# D3379 INITSINK (D3436): the InitPlan locks its config row (the M4 pair): s1 moved min_prio to 7, the InitPlan's lock
# waits and rechecks it (7 8); s1 moved row 5 to worker 7, the statement's lock refetches the worker (w7); with s2's locks
# held the config row is s2's (55P03). D3379 INITCTE: a CTE only the InitPlan reads inserts once; the recheck of row 5
# (moved out of the filter) never inserts again (6 7).
permutation s2ix s1q s2il s1c
permutation s1j s2il s1c
permutation s2b s2il s1nq s2c s1c
permutation s2cx s1p s2ic s1c s2log

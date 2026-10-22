# AGA-35 form 4 (lockrows FAMLIMIT beside an InitPlan): a LIMIT / OFFSET above a row lock over a Sort of a partitioned
# family's Append (Limit -> LockRows -> Sort -> Append -> each leaf's Seq Scan) whose leaves' filters read an InitPlan, the
# job queue's pop over a partitioned table. When s1 changed a row (READ COMMITTED), EvalPlanQual reruns Sort -> Append over
# the newest version in the leaf the row's tableoid names (the other leaves' marks inactive) with the InitPlan's value
# copied from the main run (EvalPlanQualStart): a row whose newest version still passes is returned with its new values;
# one that fails is skipped and the next row taken; one s1 moved to another leaf raises 40001 (the tuple to be locked was
# already moved to another partition); a config row s1 changed does not
# move the copied value; SKIP LOCKED takes the next row past one s1 holds; NOWAIT raises 55P03; a VOLATILE InitPlan read
# at the first row is called once; one read only in an untaken CASE arm is pending at the recheck and PG forces it there
# (Gravity refuses that form by name). Which rows the statement holds (main 2026-10-06T00:02:51Z): with s2's locks held in
# an open transaction, s1's NOWAIT probe of the row after the LIMIT succeeds and of a returned row fails; under OFFSET the
# skipped row is locked too (LockRows runs under the Limit). Expected output is the C oracle's (PostgreSQL 18, pgdg).
setup
{
  CREATE TABLE fjob (id int NOT NULL, prio int NOT NULL, v text NOT NULL) PARTITION BY RANGE (id);
  CREATE TABLE fjob1 PARTITION OF fjob FOR VALUES FROM (1) TO (13);
  CREATE TABLE fjob2 (v text NOT NULL, gone int, prio int NOT NULL, id int NOT NULL);
  ALTER TABLE fjob2 DROP COLUMN gone;
  ALTER TABLE fjob ATTACH PARTITION fjob2 FOR VALUES FROM (13) TO (25);
  CREATE TABLE fjobd PARTITION OF fjob DEFAULT;
  INSERT INTO fjob SELECT i, 100 - i, 'v' || i FROM generate_series(1, 30) i;
  CREATE TABLE fcfg (name text PRIMARY KEY, min_prio int NOT NULL);
  INSERT INTO fcfg VALUES ('q', 80);
  CREATE SEQUENCE fsq;
  CREATE TABLE flog (id int NOT NULL, v text NOT NULL);
  ANALYZE fjob, fcfg;
}
teardown { DROP TABLE fjob, fcfg, flog; DROP SEQUENCE fsq; }

session s1
setup		{ BEGIN; }
step s1v	{ UPDATE fjob SET v = v || '+' WHERE id = 20; }
step s1p	{ UPDATE fjob SET prio = 10, v = v || 'p' WHERE id = 20; }
step s1m	{ UPDATE fjob SET id = 5, v = v || 'm' WHERE id = 20; }
step s1q	{ UPDATE fcfg SET min_prio = 95 WHERE name = 'q'; }
step s1l	{ SELECT id FROM fjob WHERE id = 20 FOR UPDATE; }
step s1nq	{ SELECT name FROM fcfg WHERE name = 'q' FOR UPDATE NOWAIT; }
step s1n17	{ SELECT id FROM fjob WHERE id = 17 FOR UPDATE NOWAIT; }
step s1n18	{ SELECT id FROM fjob WHERE id = 18 FOR UPDATE NOWAIT; }
step s1n20	{ SELECT id FROM fjob WHERE id = 20 FOR UPDATE NOWAIT; }
step s1c	{ COMMIT; }
teardown	{ ROLLBACK; }

session s2
setup		{ SET enable_indexscan = off; SET enable_bitmapscan = off; }
step s2b	{ BEGIN; }
step s2c	{ COMMIT; }
step s2x	{ EXPLAIN (COSTS OFF) SELECT id, prio, v FROM fjob WHERE prio >= (SELECT min_prio FROM fcfg WHERE name = 'q') ORDER BY prio, id LIMIT 3 FOR UPDATE; }
step s2l	{ SELECT id, prio, v FROM fjob WHERE prio >= (SELECT min_prio FROM fcfg WHERE name = 'q') ORDER BY prio, id LIMIT 3 FOR UPDATE; }
step s2o	{ SELECT id, prio, v FROM fjob WHERE prio >= (SELECT min_prio FROM fcfg WHERE name = 'q') ORDER BY prio, id LIMIT 2 OFFSET 1 FOR NO KEY UPDATE; }
step s2sk	{ SELECT id, prio, v FROM fjob WHERE prio >= (SELECT min_prio FROM fcfg WHERE name = 'q') ORDER BY prio, id LIMIT 1 FOR UPDATE SKIP LOCKED; }
step s2nw	{ SELECT id, prio, v FROM fjob WHERE prio >= (SELECT min_prio FROM fcfg WHERE name = 'q') ORDER BY prio, id LIMIT 1 FOR UPDATE NOWAIT; }
step s2n	{ SELECT id, prio, v FROM fjob WHERE prio > (SELECT nextval('fsq'))::int + 78 ORDER BY prio, id LIMIT 2 FOR UPDATE; }
step s2case	{ SELECT id, prio, v FROM fjob WHERE CASE WHEN prio > 0 THEN prio >= 80 ELSE prio > (SELECT nextval('fsq'))::int END ORDER BY prio, id LIMIT 2 FOR UPDATE; }
step s2seq	{ SELECT last_value, is_called FROM fsq; }
step s2t	{ SELECT tableoid::regclass, id, v FROM fjob WHERE id IN (5, 20) ORDER BY id; }
step s2ix	{ EXPLAIN (COSTS OFF) SELECT id, prio, v FROM fjob WHERE prio >= (SELECT min_prio FROM fcfg WHERE name = 'q' FOR UPDATE) ORDER BY prio, id LIMIT 2 FOR UPDATE; }
step s2il	{ SELECT id, prio, v FROM fjob WHERE prio >= (SELECT min_prio FROM fcfg WHERE name = 'q' FOR UPDATE) ORDER BY prio, id LIMIT 2 FOR UPDATE; }
step s2cx	{ EXPLAIN (COSTS OFF) WITH w AS (INSERT INTO flog VALUES (1, 'x') RETURNING id) SELECT id, prio, v FROM fjob WHERE prio >= (SELECT min(id) FROM w) + 79 ORDER BY prio, id LIMIT 2 FOR UPDATE; }
step s2ic	{ WITH w AS (INSERT INTO flog VALUES (1, 'x') RETURNING id) SELECT id, prio, v FROM fjob WHERE prio >= (SELECT min(id) FROM w) + 79 ORDER BY prio, id LIMIT 2 FOR UPDATE; }
step s2log	{ SELECT count(*), min(id) FROM flog; }

# prio >= 80 by prio: 20 (80, fjob2), 19 (81), 18 (82), 17 (83), ...; 1..12 in fjob1, 13..24 in fjob2, 25..30 in fjobd
permutation s2x s1v s2l s1c
permutation s1p s2l s1c
permutation s1m s2l s1c s2t
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
# D3379 INITSINK (D3436): the InitPlan locks its config row (the M4 pair): s1 moved min_prio to 95, the InitPlan's lock
# waits and rechecks it (5 4); s1 changed row 20 or moved it to fjob1 as id 5, the statement's lock rechecks the arm its
# tableoid names; with s2's locks held the config row is s2's (55P03). D3379 INITCTE: a CTE only the InitPlan reads inserts
# once; the recheck of row 20 (moved out of the filter) never inserts again.
permutation s2ix s1q s2il s1c
permutation s1v s2il s1c
permutation s1m s2il s1c s2t
permutation s2b s2il s1nq s2c s1c
permutation s2cx s1p s2ic s1c s2log

# AGA-35 form 3 (lockrows REFBUILD beside an InitPlan): a row lock whose locked relation is on the build side of the inner
# hash join its pipeline probes, the probe side REFERENCE-marked (LockRows -> Hash Join -> (Seq Scan rja, Hash -> the
# locked table's scan, or Append -> a family's leaves)), one table under a LIMIT / OFFSET or under a Sort, or a partitioned
# family, its quals reading an InitPlan. When s1 changed a locked row (READ COMMITTED), EvalPlanQual reruns the join over
# the newest version of the locked row and the REFERENCE row refetched at its junk ctid (the version the main run joined,
# EvalPlanQualFetchRowMark), with the InitPlan's value copied from the main run (EvalPlanQualStart): a row whose newest
# version still passes is returned with its new values; one that fails the filter or whose join key moved is skipped and
# the next row taken; a config row s1 changed does not move the copied value; a REFERENCE row s1 changed is read as the
# main run saw it in the probe side's filter; SKIP LOCKED takes the next row past one s1 holds; NOWAIT raises 55P03; a
# VOLATILE InitPlan read at the first row is called once; one read only in an untaken CASE arm is pending at the recheck
# and PG forces it there (Gravity refuses that form by name). Which rows the statement holds (main 2026-10-06T00:02:51Z):
# with s2's locks held in an open transaction, s1's NOWAIT probe of the row after the LIMIT succeeds, of a returned row
# fails, and of the REFERENCE row it joined succeeds (a REFERENCE mark locks nothing); under OFFSET the skipped row is
# locked too (LockRows runs under the Limit); a family's every returned leaf row is locked, a row its filter drops is not.
# Expected output is the C oracle's (PostgreSQL 18, pgdg).
setup
{
  CREATE TABLE rja (id int PRIMARY KEY, owner text NOT NULL, bal int NOT NULL);
  INSERT INTO rja SELECT i, 'o' || (i % 7), (i * 37) % 101 FROM generate_series(1, 400) i;
  CREATE TABLE rjt (id int PRIMARY KEY, w int NOT NULL, note text NOT NULL);
  INSERT INTO rjt SELECT i, i % 5, 'n' || i FROM generate_series(1, 60) i;
  CREATE TABLE rjp (id int NOT NULL, v int NOT NULL, note text NOT NULL) PARTITION BY RANGE (id);
  CREATE TABLE rjp_a PARTITION OF rjp FOR VALUES FROM (1) TO (30);
  CREATE TABLE rjp_d PARTITION OF rjp DEFAULT;
  INSERT INTO rjp SELECT i, i % 10, 'p' || i FROM generate_series(1, 60) i;
  CREATE TABLE rjc (name text PRIMARY KEY, w int NOT NULL, min_bal int NOT NULL);
  INSERT INTO rjc VALUES ('q', 2, 30);
  CREATE SEQUENCE rjsq;
  ANALYZE rja, rjt, rjp, rjc;
}
teardown { DROP TABLE rja, rjt, rjp, rjc; DROP SEQUENCE rjsq; }

session s1
setup		{ BEGIN; }
step s1v	{ UPDATE rjt SET note = note || '+' WHERE id IN (7, 57); }
step s1w	{ UPDATE rjt SET w = 3, note = note || 'w' WHERE id IN (7, 57); }
step s1k	{ UPDATE rjt SET id = 1007 WHERE id = 7; }
step s1q	{ UPDATE rjc SET w = 3, min_bal = 80 WHERE name = 'q'; }
step s1a	{ UPDATE rja SET bal = 0, owner = owner || '+' WHERE id IN (7, 12); }
step s1pv	{ UPDATE rjp SET note = note || '+' WHERE id IN (12, 42); }
step s1pw	{ UPDATE rjp SET v = 3, note = note || 'w' WHERE id = 42; }
step s1l	{ SELECT id FROM rjt WHERE id = 2 FOR UPDATE; }
step s1n2	{ SELECT id FROM rjt WHERE id = 2 FOR UPDATE NOWAIT; }
step s1n12	{ SELECT id FROM rjt WHERE id = 12 FOR UPDATE NOWAIT; }
step s1n17	{ SELECT id FROM rjt WHERE id = 17 FOR UPDATE NOWAIT; }
step s1a7	{ SELECT id FROM rja WHERE id = 7 FOR UPDATE NOWAIT; }
step s1p42	{ SELECT id FROM rjp WHERE id = 42 FOR UPDATE NOWAIT; }
step s1p43	{ SELECT id FROM rjp WHERE id = 43 FOR UPDATE NOWAIT; }
step s1c	{ COMMIT; }
teardown	{ ROLLBACK; }

session s2
setup		{ SET enable_indexscan = off; SET enable_bitmapscan = off; SET enable_nestloop = off; SET enable_mergejoin = off; }
step s2b	{ BEGIN; }
step s2c	{ COMMIT; }
step s2x	{ EXPLAIN (COSTS OFF) SELECT t.id, t.w, t.note, a.bal FROM rjt t JOIN rja a ON a.id = t.id WHERE t.w = (SELECT w FROM rjc WHERE name = 'q') LIMIT 3 FOR UPDATE OF t; }
step s2l	{ SELECT t.id, t.w, t.note, a.bal FROM rjt t JOIN rja a ON a.id = t.id WHERE t.w = (SELECT w FROM rjc WHERE name = 'q') LIMIT 3 FOR UPDATE OF t; }
step s2px	{ EXPLAIN (COSTS OFF) SELECT t.id, t.note, a.owner, a.bal FROM rjt t JOIN rja a ON a.id = t.id WHERE t.w = 2 AND a.bal >= (SELECT min_bal FROM rjc WHERE name = 'q') LIMIT 3 FOR UPDATE OF t; }
step s2p	{ SELECT t.id, t.note, a.owner, a.bal FROM rjt t JOIN rja a ON a.id = t.id WHERE t.w = 2 AND a.bal >= (SELECT min_bal FROM rjc WHERE name = 'q') LIMIT 3 FOR UPDATE OF t; }
step s2sx	{ EXPLAIN (COSTS OFF) SELECT t.id, t.w, t.note, a.bal FROM rjt t JOIN rja a ON a.id = t.id WHERE t.w = (SELECT w FROM rjc WHERE name = 'q') ORDER BY t.id DESC LIMIT 2 FOR UPDATE OF t; }
step s2s	{ SELECT t.id, t.w, t.note, a.bal FROM rjt t JOIN rja a ON a.id = t.id WHERE t.w = (SELECT w FROM rjc WHERE name = 'q') ORDER BY t.id DESC LIMIT 2 FOR UPDATE OF t; }
step s2o	{ SELECT t.id, t.w, t.note FROM rjt t JOIN rja a ON a.id = t.id WHERE t.w = (SELECT w FROM rjc WHERE name = 'q') LIMIT 2 OFFSET 1 FOR NO KEY UPDATE OF t; }
step s2sk	{ SELECT t.id, t.note FROM rjt t JOIN rja a ON a.id = t.id WHERE t.w = (SELECT w FROM rjc WHERE name = 'q') LIMIT 1 FOR UPDATE OF t SKIP LOCKED; }
step s2nw	{ SELECT t.id, t.note FROM rjt t JOIN rja a ON a.id = t.id WHERE t.w = (SELECT w FROM rjc WHERE name = 'q') LIMIT 1 FOR UPDATE OF t NOWAIT; }
step s2n	{ SELECT t.id, t.w, t.note FROM rjt t JOIN rja a ON a.id = t.id WHERE t.w = (SELECT nextval('rjsq'))::int + 1 LIMIT 2 FOR UPDATE OF t; }
step s2case	{ SELECT t.id, t.w, t.note FROM rjt t JOIN rja a ON a.id = t.id WHERE CASE WHEN t.id > 0 THEN t.w = 2 ELSE t.w = (SELECT nextval('rjsq'))::int END LIMIT 2 FOR UPDATE OF t; }
step s2seq	{ SELECT last_value, is_called FROM rjsq; }
step s2fx	{ EXPLAIN (COSTS OFF) SELECT p.id, p.v, p.note, a.bal FROM rjp p JOIN rja a ON a.id = p.id WHERE p.v = 2 AND a.bal > (SELECT w FROM rjc WHERE name = 'q') FOR UPDATE OF p; }
step s2f	{ SELECT p.id, p.v, p.note, a.bal FROM rjp p JOIN rja a ON a.id = p.id WHERE p.v = 2 AND a.bal > (SELECT w FROM rjc WHERE name = 'q') FOR UPDATE OF p; }
step s2fl	{ SELECT p.id, p.v, p.note, a.bal FROM rjp p JOIN rja a ON a.id = p.id WHERE p.v = (SELECT w FROM rjc WHERE name = 'q') FOR UPDATE OF p; }
step s2fp	{ SELECT p.id, p.note, a.owner, a.bal FROM rjp p JOIN rja a ON a.id = p.id WHERE p.v = 2 AND a.bal >= (SELECT min_bal FROM rjc WHERE name = 'q') FOR SHARE OF p; }

# w = 2 in rjt: 2 7 12 17 22 .. 57 (probed in rja's order: by id); bal (id * 37 % 101): 2 74, 7 57, 12 40, 17 23, 22 6, 27 90
# v = 2 in rjp: 2 12 22 (rjp_a) 32 42 52 (rjp_d); bal: 2 74, 12 40, 22 6, 32 73, 42 39, 52 5 (s2f: bal > 2 takes all six;
# s2fl's leaves read the InitPlan: Gravity refuses that union by name, D1486)
permutation s2x s1v s2l s1c
permutation s1w s2l s1c
permutation s1k s2l s1c
permutation s1q s1v s2l s1c
permutation s2px s1a s1v s2p s1c
permutation s2sx s1v s2s s1c
permutation s1w s2s s1c
permutation s1w s2o s1c
permutation s1l s2sk s1c
permutation s1l s2nw s1c
permutation s1v s2n s1c s2seq
permutation s1v s2case s1c s2seq
permutation s2fx s1pv s2f s1c
permutation s1pw s2f s1c
permutation s1pv s2fl s1c
permutation s1a s1pv s2fp s1c
# s2 holds its locks: LIMIT 3 locks 2 7 12 (17, the 4th, is free; rja 7, a REFERENCE row it joined, is not locked);
# LIMIT 2 OFFSET 1 locks 2 (the OFFSET row) 7 12; the family form locks its six v = 2 rows (43, v = 3, is free)
permutation s2b s2l s1n17 s1a7 s1n12 s2c s1c
permutation s2b s2o s1n17 s1n2 s2c s1c
permutation s2b s2f s1p43 s1p42 s2c s1c

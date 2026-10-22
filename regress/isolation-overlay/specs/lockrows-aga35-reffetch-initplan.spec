# AGA-35 form 2 (lockrows REFFETCH beside an InitPlan): a LIMIT / OFFSET above a row lock over a Sort of a hash join whose
# probe side is the locked relation and whose hashed side is REFERENCE-marked (Limit -> LockRows -> Sort -> Hash Join,
# FOR UPDATE OF j), its quals reading an InitPlan. When s1 changed a locked row (READ COMMITTED), EvalPlanQual reruns the
# join over the newest version of the locked row and the REFERENCE row refetched at its ctid (the version the main run
# joined, EvalPlanQualFetchRowMark), with the InitPlan's value copied from the main run (EvalPlanQualStart): a row whose
# newest version still passes is returned with its new values; one that fails the filter or no longer joins the refetched
# row is skipped and the next row taken; a config row s1 changed does not move the copied value; a REFERENCE row s1
# changed is read as the main run saw it, in the probe side's filter and in its own; SKIP LOCKED takes the next row past
# one s1 holds; NOWAIT raises 55P03; a VOLATILE InitPlan read at the first row is called once; one read only in an untaken
# CASE arm is pending at the recheck and PG forces it there (Gravity refuses that form by name). Which rows the statement
# holds (main 2026-10-06T00:02:51Z): with s2's locks held in an open transaction, s1's NOWAIT probe of the row after the
# LIMIT succeeds, of a returned row fails, and of the REFERENCE row it joined succeeds (a REFERENCE mark locks nothing);
# under OFFSET the skipped row is locked too (LockRows runs under the Limit). Expected output is the C oracle's
# (PostgreSQL 18, pgdg).
setup
{
  CREATE TABLE bjob (id int PRIMARY KEY, grp int NOT NULL, prio int NOT NULL, v text NOT NULL);
  INSERT INTO bjob SELECT i, i % 4, 100 - i, 'v' || i FROM generate_series(1, 24) i;
  CREATE TABLE bgrp (grp int PRIMARY KEY, label text NOT NULL, w int NOT NULL);
  INSERT INTO bgrp SELECT g, 'g' || g, g * 10 + 10 FROM generate_series(0, 3) g;
  CREATE TABLE bcfg (name text PRIMARY KEY, min_prio int NOT NULL, min_w int NOT NULL);
  INSERT INTO bcfg VALUES ('q', 80, 10);
  CREATE SEQUENCE bsq;
  CREATE TABLE blog (id int NOT NULL, v text NOT NULL);
  ANALYZE bjob, bgrp, bcfg;
}
teardown { DROP TABLE bjob, bgrp, bcfg, blog; DROP SEQUENCE bsq; }

session s1
setup		{ BEGIN; }
step s1v	{ UPDATE bjob SET v = v || '+' WHERE id = 20; }
step s1p	{ UPDATE bjob SET prio = 10, v = v || 'p' WHERE id = 20; }
step s1j	{ UPDATE bjob SET grp = 1, v = v || 'j' WHERE id = 20; }
step s1q	{ UPDATE bcfg SET min_prio = 95, min_w = 40 WHERE name = 'q'; }
step s1g	{ UPDATE bgrp SET w = 0, label = label || '+' WHERE grp = 0; }
step s1l	{ SELECT id FROM bjob WHERE id = 20 FOR UPDATE; }
step s1n17	{ SELECT id FROM bjob WHERE id = 17 FOR UPDATE NOWAIT; }
step s1n18	{ SELECT id FROM bjob WHERE id = 18 FOR UPDATE NOWAIT; }
step s1n20	{ SELECT id FROM bjob WHERE id = 20 FOR UPDATE NOWAIT; }
step s1g0	{ SELECT grp FROM bgrp WHERE grp = 0 FOR UPDATE NOWAIT; }
step s1nq	{ SELECT name FROM bcfg WHERE name = 'q' FOR UPDATE NOWAIT; }
step s1c	{ COMMIT; }
teardown	{ ROLLBACK; }

session s2
setup		{ SET enable_indexscan = off; SET enable_bitmapscan = off; SET enable_nestloop = off; SET enable_mergejoin = off; }
step s2b	{ BEGIN; }
step s2c	{ COMMIT; }
step s2ix	{ EXPLAIN (COSTS OFF) SELECT j.id, j.prio, j.v, g.label FROM bjob j JOIN bgrp g ON g.grp = j.grp WHERE j.prio >= (SELECT min_prio FROM bcfg WHERE name = 'q' FOR UPDATE) ORDER BY j.prio, j.id LIMIT 2 FOR UPDATE OF j; }
step s2il	{ SELECT j.id, j.prio, j.v, g.label FROM bjob j JOIN bgrp g ON g.grp = j.grp WHERE j.prio >= (SELECT min_prio FROM bcfg WHERE name = 'q' FOR UPDATE) ORDER BY j.prio, j.id LIMIT 2 FOR UPDATE OF j; }
step s2cx	{ EXPLAIN (COSTS OFF) WITH w AS (INSERT INTO blog VALUES (1, 'x') RETURNING id) SELECT j.id, j.prio, j.v, g.label FROM bjob j JOIN bgrp g ON g.grp = j.grp WHERE j.prio >= (SELECT min(id) FROM w) + 79 ORDER BY j.prio, j.id LIMIT 2 FOR UPDATE OF j; }
step s2ic	{ WITH w AS (INSERT INTO blog VALUES (1, 'x') RETURNING id) SELECT j.id, j.prio, j.v, g.label FROM bjob j JOIN bgrp g ON g.grp = j.grp WHERE j.prio >= (SELECT min(id) FROM w) + 79 ORDER BY j.prio, j.id LIMIT 2 FOR UPDATE OF j; }
step s2log	{ SELECT count(*), min(id) FROM blog; }
step s2x	{ EXPLAIN (COSTS OFF) SELECT j.id, j.prio, j.v, g.label FROM bjob j JOIN bgrp g ON g.grp = j.grp WHERE j.prio >= (SELECT min_prio FROM bcfg WHERE name = 'q') ORDER BY j.prio, j.id LIMIT 3 FOR UPDATE OF j; }
step s2l	{ SELECT j.id, j.prio, j.v, g.label FROM bjob j JOIN bgrp g ON g.grp = j.grp WHERE j.prio >= (SELECT min_prio FROM bcfg WHERE name = 'q') ORDER BY j.prio, j.id LIMIT 3 FOR UPDATE OF j; }
step s2rx	{ EXPLAIN (COSTS OFF) SELECT j.id, j.prio, j.v, g.w FROM bjob j JOIN bgrp g ON g.grp = j.grp AND g.w >= (SELECT min_w FROM bcfg WHERE name = 'q') WHERE j.prio >= 80 ORDER BY j.prio, j.id LIMIT 3 FOR UPDATE OF j; }
step s2r	{ SELECT j.id, j.prio, j.v, g.w FROM bjob j JOIN bgrp g ON g.grp = j.grp AND g.w >= (SELECT min_w FROM bcfg WHERE name = 'q') WHERE j.prio >= 80 ORDER BY j.prio, j.id LIMIT 3 FOR UPDATE OF j; }
step s2o	{ SELECT j.id, j.prio, j.v, g.label FROM bjob j JOIN bgrp g ON g.grp = j.grp WHERE j.prio >= (SELECT min_prio FROM bcfg WHERE name = 'q') ORDER BY j.prio, j.id LIMIT 2 OFFSET 1 FOR NO KEY UPDATE OF j; }
step s2sk	{ SELECT j.id, j.prio, j.v, g.label FROM bjob j JOIN bgrp g ON g.grp = j.grp WHERE j.prio >= (SELECT min_prio FROM bcfg WHERE name = 'q') ORDER BY j.prio, j.id LIMIT 1 FOR UPDATE OF j SKIP LOCKED; }
step s2nw	{ SELECT j.id, j.prio, j.v, g.label FROM bjob j JOIN bgrp g ON g.grp = j.grp WHERE j.prio >= (SELECT min_prio FROM bcfg WHERE name = 'q') ORDER BY j.prio, j.id LIMIT 1 FOR UPDATE OF j NOWAIT; }
step s2n	{ SELECT j.id, j.prio, j.v, g.label FROM bjob j JOIN bgrp g ON g.grp = j.grp WHERE j.prio > (SELECT nextval('bsq'))::int + 78 ORDER BY j.prio, j.id LIMIT 2 FOR UPDATE OF j; }
step s2case	{ SELECT j.id, j.prio, j.v, g.label FROM bjob j JOIN bgrp g ON g.grp = j.grp WHERE CASE WHEN j.prio > 0 THEN j.prio >= 80 ELSE j.prio > (SELECT nextval('bsq'))::int END ORDER BY j.prio, j.id LIMIT 2 FOR UPDATE OF j; }
step s2seq	{ SELECT last_value, is_called FROM bsq; }

# prio >= 80 by prio: 20 (80, grp 0), 19 (81, grp 3), 18 (82, grp 2), 17 (83, grp 1), ...
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
# s2 holds its locks: LIMIT 3 locks 20 19 18 (17, the 4th, is free; bgrp 0, row 20's REFERENCE row, is not locked);
# LIMIT 2 OFFSET 1 locks 20 (the OFFSET row) 19 18
permutation s2b s2l s1n17 s1g0 s1n18 s2c s1c
permutation s2b s2o s1n17 s1n20 s2c s1c
# D3379 INITSINK (D3436): the InitPlan locks its config row (the M4 pair): s1 moved min_prio to 95, the InitPlan's lock
# waits and rechecks it (5 4); s1 moved row 20 to group 1, the statement's lock refetches the group (g1); with s2's locks
# held the config row is s2's (55P03). D3379 INITCTE: a CTE only the InitPlan reads inserts once; the recheck of row 20
# (moved out of the filter) never inserts again.
permutation s2ix s1q s2il s1c
permutation s1j s2il s1c
permutation s2b s2il s1nq s2c s1c
permutation s2cx s1p s2ic s1c s2log

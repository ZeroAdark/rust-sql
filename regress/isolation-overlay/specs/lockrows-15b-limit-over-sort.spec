# lockrows 15b: a LIMIT / OFFSET above a row lock over a Sort of the locked relation's own scan (Limit -> LockRows -> Sort
# -> Seq Scan). When s1 changed a row (READ COMMITTED), EvalPlanQual reruns Sort -> scan over the newest version and
# LockRows hands that row to the Limit: it is counted where it stands in the sorted order (new values, not re-sorted); a
# row whose newest version fails the qual is skipped and the next row taken; under OFFSET a skipped-by-offset row is
# locked and rechecked but not returned; SKIP LOCKED skips a row s1 holds and the LIMIT takes the next one (a queue's
# pop); NOWAIT raises 55P03. Expected output is the C oracle's (PostgreSQL 18, pgdg).
setup
{
  CREATE TABLE lsl (id int PRIMARY KEY, grp int NOT NULL, k int NOT NULL, v text NOT NULL);
  INSERT INTO lsl SELECT i, i % 3, 100 - i, 'v' || i FROM generate_series(1, 24) i;
  ANALYZE lsl;
}
teardown { DROP TABLE lsl; }

session s1
setup		{ BEGIN; }
step s1v	{ UPDATE lsl SET v = v || '+' WHERE id = 19; }
step s1k	{ UPDATE lsl SET k = 0, v = v || 'k' WHERE id = 19; }
step s1g	{ UPDATE lsl SET grp = 2 WHERE id = 19; }
step s1l	{ SELECT id FROM lsl WHERE id = 22 FOR UPDATE; }
step s1c	{ COMMIT; }
teardown	{ ROLLBACK; }

session s2
setup		{ SET enable_indexscan = off; SET enable_bitmapscan = off; }
step s2x	{ EXPLAIN (COSTS OFF) SELECT id, k, v FROM lsl WHERE grp = 1 ORDER BY k LIMIT 3 FOR UPDATE; }
step s2l	{ SELECT id, k, v FROM lsl WHERE grp = 1 ORDER BY k LIMIT 3 FOR UPDATE; }
step s2o	{ SELECT id, k, v FROM lsl WHERE grp = 1 ORDER BY k LIMIT 2 OFFSET 1 FOR NO KEY UPDATE; }
step s2o2	{ SELECT id, k, v FROM lsl WHERE grp = 1 ORDER BY k LIMIT 2 OFFSET 2 FOR SHARE; }
step s2sk	{ SELECT id, k, v FROM lsl WHERE grp = 1 ORDER BY k LIMIT 2 FOR UPDATE SKIP LOCKED; }
step s2nw	{ SELECT id, k, v FROM lsl WHERE grp = 1 ORDER BY k LIMIT 1 FOR UPDATE NOWAIT; }
step s2t	{ SELECT id, v, k * 2 AS k2 FROM lsl WHERE grp = 1 AND v <> 'none' ORDER BY v DESC LIMIT 4 FOR UPDATE; }

# grp = 1 by k: 22 (78), 19 (81), 16 (84), 13 (87), ...
permutation s2x s1v s2l s1c
permutation s1k s2l s1c
permutation s1g s2l s1c
permutation s1v s2o s1c
permutation s1g s2o s1c
permutation s1g s2o2 s1c
permutation s1v s2t s1c
permutation s1l s2sk s1c
permutation s1l s1v s2sk s1c
permutation s1l s2nw s1c

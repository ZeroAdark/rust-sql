# lockrows 15 (CNM-38): a row lock over a Sort (LockRows -> Sort -> a scan or a hash join). ExecLockRows locks each
# row by its junk ctid as the Sort hands it on; when s1 changed the row (READ COMMITTED), EvalPlanQual reruns the plan
# below LockRows with the newest version substituted (the Sort over that one row) and LockRows returns that row in its
# sorted place: the new values, never re-sorted (s1k moves the sort key); a row whose newest version fails a qual, or
# a deleted one, is skipped; a REFERENCE-marked relation's row is the version the join read (FOR UPDATE OF a), a
# locked one's its newest (FOR UPDATE of both); a subquery's row (a COPY mark) is the one the join read, travelling whole
# (EvalPlanQualFetchRowMark's copy leg: never re-evaluated), so a locked row whose join key moved fails the join and is
# skipped. SKIP LOCKED skips the row, NOWAIT raises 55P03, REPEATABLE READ 40001.
# Each permutation changes its own row (the commits stay): s1v row 4, s1k 13, s1g 7, s1d 10, s1vw 16, s1w lsb's 19,
# s1cv row 1 and lsb's 1, s1ci row 22 (its id moves to 122).
# Expected output is the C oracle's (PostgreSQL 18, pgdg).

setup
{
  CREATE TABLE lsa (id int PRIMARY KEY, grp int NOT NULL, k int NOT NULL, v text NOT NULL);
  INSERT INTO lsa SELECT i, i % 3, 100 - i, 'v' || i FROM generate_series(1, 24) i;
  CREATE TABLE lsb (id int PRIMARY KEY, a_id int NOT NULL, w int NOT NULL);
  INSERT INTO lsb SELECT i, i, i * 10 FROM generate_series(1, 24) i;
  ANALYZE lsa, lsb;
}

teardown
{
  DROP TABLE lsb;
  DROP TABLE lsa;
}

session s1
setup		{ BEGIN; }
step s1v	{ UPDATE lsa SET v = v || '+' WHERE id = 4; }
step s1k	{ UPDATE lsa SET k = 0, v = v || 'k' WHERE id = 13; }
step s1g	{ UPDATE lsa SET grp = 2 WHERE id = 7; }
step s1d	{ DELETE FROM lsa WHERE id = 10; }
step s1vw	{ UPDATE lsa SET v = v || '+' WHERE id = 16; UPDATE lsb SET w = w + 1 WHERE id = 16; }
step s1w	{ UPDATE lsb SET w = 0 WHERE id = 19; }
step s1cv	{ UPDATE lsa SET v = v || '+' WHERE id = 1; UPDATE lsb SET w = -1 WHERE id = 1; }
step s1ci	{ UPDATE lsa SET id = 122 WHERE id = 22; }
step s1c	{ COMMIT; }
step s1a	{ ROLLBACK; }
teardown	{ ROLLBACK; }

session s2
setup		{ SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_indexscan = off; SET enable_bitmapscan = off; }
step s2x	{ EXPLAIN (COSTS OFF) SELECT id, k, v FROM lsa WHERE grp = 1 ORDER BY k FOR UPDATE; }
step s2s	{ SELECT id, k, v FROM lsa WHERE grp = 1 ORDER BY k FOR UPDATE; }
step s2sh	{ SELECT id, k, v FROM lsa WHERE grp = 1 ORDER BY k, v FOR SHARE; }
step s2xj	{ EXPLAIN (COSTS OFF) SELECT a.id, a.v, b.w FROM lsa a JOIN lsb b ON b.a_id = a.id WHERE a.grp = 1 ORDER BY a.k FOR UPDATE OF a; }
step s2j	{ SELECT a.id, a.v, b.w FROM lsa a JOIN lsb b ON b.a_id = a.id WHERE a.grp = 1 ORDER BY a.k FOR UPDATE OF a; }
step s2b	{ SELECT a.id, a.v, b.w FROM lsa a JOIN lsb b ON b.a_id = a.id WHERE a.grp = 1 ORDER BY a.k FOR UPDATE; }
step s2xc	{ EXPLAIN (COSTS OFF) SELECT a.id, a.v, q.w FROM lsa a, (SELECT a_id, w FROM lsb OFFSET 0) q WHERE q.a_id = a.id AND a.grp = 1 ORDER BY a.k FOR UPDATE OF a; }
step s2c	{ SELECT a.id, a.v, q.w FROM lsa a, (SELECT a_id, w FROM lsb OFFSET 0) q WHERE q.a_id = a.id AND a.grp = 1 ORDER BY a.k FOR UPDATE OF a; }
step s2sk	{ SELECT id, k, v FROM lsa WHERE grp = 1 ORDER BY k FOR UPDATE SKIP LOCKED; }
step s2nw	{ SELECT id, k, v FROM lsa WHERE grp = 1 ORDER BY k FOR UPDATE NOWAIT; }

session s3
setup		{ BEGIN ISOLATION LEVEL REPEATABLE READ; SELECT count(*) FROM lsa; }
step s3s	{ SELECT id, k, v FROM lsa WHERE grp = 1 ORDER BY k FOR UPDATE; }
teardown	{ ROLLBACK; }

# the newest version's values, in the row's sorted place
permutation s2x s1v s2s s1c
# the sort key moved: the row keeps its sorted place (no re-sort)
permutation s1k s2s s1c
permutation s1k s2sh s1c
# no longer grp = 1: skipped; deleted: skipped; s1 rolls back: the row as it was
permutation s1g s2s s1c
permutation s1d s2s s1c
permutation s1v s2s s1a
# over a hash join: the REFERENCE mark's row is the version the join read; both locked: each one's newest
permutation s2xj s1vw s2j s1c
permutation s1vw s2b s1c
permutation s1w s2b s1c
# a subquery's COPY mark: its row as the join read it (lsb's new w not seen); the locked row's id moved: skipped
permutation s2xc s1cv s2c s1c
permutation s1ci s2c s1c
# SKIP LOCKED skips the locked row; NOWAIT raises
permutation s1v s2sk s1c
permutation s1v s2nw s1c
# REPEATABLE READ: the concurrent update is a serialization failure
permutation s1v s3s s1c

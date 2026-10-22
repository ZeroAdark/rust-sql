# lockrows D3458 CLAIMALL (m4_iso: `DELETE FROM iso_job WHERE id IN (SELECT id FROM iso_job WHERE owner = 'A' FOR UPDATE
# NOWAIT)`): the job-queue claim's IN form with no LIMIT. PostgreSQL plans a Hash Semi Join, the target's scan probing the
# hashed SubqueryScan -> LockRows -> scan; the subquery's LockRows runs once into the Hash and rechecks under its own
# EvalPlanQual, and the DELETE's recheck reads the subquery's row from its COPY mark. s1 changes row 3 (owner A, so the
# subquery locks it) and commits while s2's lock waits on it:
#  s1upd: a column the subquery does not read: row 3 stays claimed and both A rows are deleted (the DELETE rechecks
#  row 3's newest version against the stored subquery row).
#  s1own: row 3 handed to B: the lock's recheck drops it, only row 4 is deleted.
#  s1id: row 3 renumbered 30: the recheck hands 30 to the Hash, which the target's scan (the statement's snapshot, row 3
#  at id 3) never matches: only row 4 is deleted.
#  s1del: row 3 deleted: the lock skips it, only row 4 is deleted.
#  NOWAIT raises on the held row (55P03).
# Expected output is PostgreSQL 18.6's (the C oracle).
setup
{
  CREATE TABLE cq (id int PRIMARY KEY, state text NOT NULL, owner text) WITH (autovacuum_enabled = off);
  INSERT INTO cq SELECT g, 'new', CASE WHEN g IN (3, 4) THEN 'A' WHEN g IN (5, 6) THEN 'B' END FROM generate_series(1, 8) g;
  ANALYZE cq;
}

teardown
{
  DROP TABLE cq;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1upd	{ UPDATE cq SET state = 'held' WHERE id = 3; }
step s1own	{ UPDATE cq SET owner = 'B' WHERE id = 3; }
step s1id	{ UPDATE cq SET id = 30 WHERE id = 3; }
step s1del	{ DELETE FROM cq WHERE id = 3; }
step s1c	{ COMMIT; }

session s2
step s2e	{ EXPLAIN (COSTS OFF) DELETE FROM cq WHERE id IN (SELECT id FROM cq WHERE owner = 'A' FOR UPDATE) RETURNING id; }
step s2d	{ DELETE FROM cq WHERE id IN (SELECT id FROM cq WHERE owner = 'A' FOR UPDATE) RETURNING id, state; }
step s2n	{ DELETE FROM cq WHERE id IN (SELECT id FROM cq WHERE owner = 'A' FOR UPDATE NOWAIT) RETURNING id; }
step s2r	{ SELECT id, state, owner FROM cq ORDER BY id; }

permutation s2e s1c
permutation s1upd s2d s1c s2r
permutation s1own s2d s1c s2r
permutation s1id s2d s1c s2r
permutation s1del s2d s1c s2r
permutation s1upd s2n s1c s2r

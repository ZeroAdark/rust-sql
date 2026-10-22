# lockrows SUBLOCK (D1018 sub-shape E, gen lk-0060 / lk-0068): a LockRows inside a correlated EXISTS SubPlan. PostgreSQL
# never pulls a sublink holding row marks up into a join, so the SubPlan keeps its own LockRows over the inner index scan
# (Index Cond prio = 9 AND id = j.id on the partial index; Filter state = 'ready', kept for EvalPlanQual). That LockRows
# locks the row the SubPlan's first pull reaches; over a newer version (READ COMMITTED) its own EvalPlanQual reruns the
# scan alone over the newest version (its index quals and filter, the outer row's parameter as read). A version that
# fails them, or a deleted row, is skipped, so EXISTS is false for that outer row; SKIP LOCKED skips a row s1 holds;
# NOWAIT raises 55P03. The statement's UPDATE then meets the row s1 changed: its own EvalPlanQual runs the SubPlan again
# over the newest target row (s2w: k is another row, read through j.ref, so a changed ref finds no row and the UPDATE
# skips j). s3 is the plain SELECT form, its lock the statement's one; s4 (D3379 SUBLOCK-LEADER) the same over a
# sequential scan, a pipeline decompose leaves parallel-eligible, run on the leader. Expected output is PostgreSQL 18.6's.
setup
{
  CREATE TABLE lsj (id int PRIMARY KEY, state text NOT NULL, prio int NOT NULL, ref int, attempts int NOT NULL DEFAULT 0);
  INSERT INTO lsj SELECT i, CASE WHEN i % 4 = 0 THEN 'done' ELSE 'ready' END, CASE WHEN i % 2 = 1 THEN 9 ELSE 1 END, i + 4, 0 FROM generate_series(1, 10) i;
  CREATE INDEX lsj_ready ON lsj (prio, id) WHERE state = 'ready';
  ANALYZE lsj;
}
teardown { DROP TABLE lsj; }

session s1
setup		{ BEGIN; }
step s1a	{ UPDATE lsj SET attempts = attempts + 100 WHERE id = 5; }
step s1s	{ UPDATE lsj SET state = 'done' WHERE id = 5; }
step s1p	{ UPDATE lsj SET prio = 2 WHERE id = 5; }
step s1d	{ DELETE FROM lsj WHERE id = 5; }
step s1l	{ SELECT id FROM lsj WHERE id = 5 FOR UPDATE; }
step s1j	{ UPDATE lsj SET attempts = attempts + 100 WHERE id = 1; }
step s1r	{ UPDATE lsj SET ref = 99 WHERE id = 1; }
step s1c	{ COMMIT; }
teardown	{ ROLLBACK; }

session s2
setup		{ SET enable_seqscan = off; SET enable_bitmapscan = off; }
step s2x	{ EXPLAIN (COSTS OFF) UPDATE lsj j SET attempts = j.attempts + 1 WHERE EXISTS (SELECT 1 FROM lsj k WHERE k.id = j.id AND k.state = 'ready' AND k.prio = 9 FOR UPDATE) RETURNING j.id, j.attempts; }
step s2u	{ UPDATE lsj j SET attempts = j.attempts + 1 WHERE EXISTS (SELECT 1 FROM lsj k WHERE k.id = j.id AND k.state = 'ready' AND k.prio = 9 FOR UPDATE) RETURNING j.id, j.attempts; }
step s2sk	{ UPDATE lsj j SET attempts = j.attempts + 1 WHERE EXISTS (SELECT 1 FROM lsj k WHERE k.id = j.id AND k.state = 'ready' AND k.prio = 9 FOR UPDATE SKIP LOCKED) RETURNING j.id, j.attempts; }
step s2nw	{ UPDATE lsj j SET attempts = j.attempts + 1 WHERE EXISTS (SELECT 1 FROM lsj k WHERE k.id = j.id AND k.state = 'ready' AND k.prio = 9 FOR UPDATE NOWAIT) RETURNING j.id, j.attempts; }
step s2w	{ UPDATE lsj j SET attempts = j.attempts + 1 WHERE j.id <= 4 AND EXISTS (SELECT 1 FROM lsj k WHERE k.id = j.ref AND k.state = 'ready' FOR UPDATE) RETURNING j.id, j.attempts; }

session s3
setup		{ SET enable_seqscan = off; SET enable_bitmapscan = off; }
step s3l	{ SELECT j.id FROM lsj j WHERE j.id <= 8 AND EXISTS (SELECT 1 FROM lsj k WHERE k.id = j.id AND k.state = 'ready' FOR SHARE); }

session s4
step s4l	{ SELECT j.id FROM lsj j WHERE j.prio = 9 AND EXISTS (SELECT 1 FROM lsj k WHERE k.id = j.id AND k.state = 'ready' FOR UPDATE); }

# ready and prio 9: ids 1, 3, 5, 7, 9; j.ref = j.id + 4 (8 is done, so s2w takes 1, 2, 3)
permutation s2x s1a s2u s1c
permutation s1s s2u s1c
permutation s1p s2u s1c
permutation s1d s2u s1c
permutation s1l s2sk s1c
permutation s1l s2nw s1c
permutation s1j s2w s1c
permutation s1r s2w s1c
permutation s1a s3l s1c
permutation s1s s3l s1c
permutation s1a s4l s1c
permutation s1s s4l s1c

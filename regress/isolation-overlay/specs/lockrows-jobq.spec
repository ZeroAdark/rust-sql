# lockrows JOBQ (lk-0055 / 0058 / 0063 / 0066): the job-queue claim, UPDATE / DELETE job .. WHERE id = (SELECT id FROM job
# WHERE state = 'ready' .. ORDER BY prio, id LIMIT 1 FOR UPDATE [SKIP LOCKED]). The InitPlan's LockRows keeps its own row
# mark and rechecks under its own EvalPlanQual: a row another transaction holds is skipped (SKIP LOCKED) or waited for
# (FOR UPDATE); after the wait the newest version is rechecked against the InitPlan's quals, a row that no longer
# qualifies (moved out of 'ready', deleted) is passed over and the Limit takes the next one in index order, and one that
# still qualifies is the claimed row, written in its newest version. The write's own recheck copies the InitPlan's value.
# The IN form (`id IN (SELECT id .. LIMIT 3 FOR UPDATE ..)`, a HashAggregate over the subquery, lk-0056 / 0059 / 0064 /
# 0067): the subquery's LockRows the same, and the write's recheck reads the stored subquery row (ROW_MARK_COPY).

setup
{
 CREATE TABLE job (id int PRIMARY KEY, queue text NOT NULL, prio int NOT NULL, state text NOT NULL, attempts int NOT NULL DEFAULT 0, note text);
 CREATE INDEX job_ready ON job (prio, id) WHERE state = 'ready';
 INSERT INTO job SELECT i, CASE WHEN i % 3 = 0 THEN 'sms' ELSE 'mail' END, i % 5, CASE WHEN i % 4 = 0 THEN 'done' ELSE 'ready' END, 0, 'n' || i FROM generate_series(1, 200) i;
 ANALYZE job;
}

teardown
{
 DROP TABLE job;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; SET enable_seqscan = off; SET enable_sort = off; }
step s1n	{ UPDATE job SET note = 'held', attempts = attempts + 10 WHERE id = 5; }
step s1d	{ UPDATE job SET state = 'done' WHERE id = 5; }
step s1x	{ DELETE FROM job WHERE id = 5; }
step s1p	{ UPDATE job SET prio = 4 WHERE id = 5; }
step s1c	{ COMMIT; }
step s1a	{ ROLLBACK; }

session s2
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; SET enable_seqscan = off; SET enable_sort = off; }
step s2e	{ EXPLAIN (COSTS OFF) UPDATE job SET state = 'running', attempts = attempts + 1 WHERE id = (SELECT id FROM job WHERE state = 'ready' AND queue = 'mail' ORDER BY prio, id LIMIT 1 FOR UPDATE) RETURNING id, queue, attempts; }
step s2u	{ UPDATE job SET state = 'running', attempts = attempts + 1 WHERE id = (SELECT id FROM job WHERE state = 'ready' AND queue = 'mail' ORDER BY prio, id LIMIT 1 FOR UPDATE) RETURNING id, queue, attempts, note; }
step s2s	{ UPDATE job SET state = 'running', attempts = attempts + 1 WHERE id = (SELECT id FROM job WHERE state = 'ready' AND queue = 'mail' ORDER BY prio, id LIMIT 1 FOR UPDATE SKIP LOCKED) RETURNING id, queue, attempts, note; }
step s2d	{ DELETE FROM job WHERE id = (SELECT id FROM job WHERE state = 'ready' AND queue = 'mail' ORDER BY prio, id LIMIT 1 FOR UPDATE) RETURNING id, note; }
step s2k	{ DELETE FROM job WHERE id = (SELECT id FROM job WHERE state = 'ready' AND queue = 'mail' ORDER BY prio, id LIMIT 1 FOR UPDATE SKIP LOCKED) RETURNING id, note; }
step s2ie	{ EXPLAIN (COSTS OFF) UPDATE job SET state = 'running' WHERE id IN (SELECT id FROM job WHERE state = 'ready' AND queue = 'mail' ORDER BY prio, id LIMIT 3 FOR UPDATE) RETURNING id; }
step s2i	{ UPDATE job SET state = 'running', attempts = attempts + 1 WHERE id IN (SELECT id FROM job WHERE state = 'ready' AND queue = 'mail' ORDER BY prio, id LIMIT 3 FOR UPDATE); }
step s2j	{ UPDATE job SET state = 'running', attempts = attempts + 1 WHERE id IN (SELECT id FROM job WHERE state = 'ready' AND queue = 'mail' ORDER BY prio, id LIMIT 3 FOR UPDATE SKIP LOCKED); }
step s2x	{ DELETE FROM job WHERE id IN (SELECT id FROM job WHERE state = 'ready' AND queue = 'mail' ORDER BY prio, id LIMIT 3 FOR UPDATE); }
step s2c	{ COMMIT; }

session s3
step s3sel	{ SELECT id, state, attempts, note FROM job WHERE id IN (5, 10, 15, 20, 25) OR state = 'running' ORDER BY id; }

permutation s2e s1c s2c
permutation s1n s2u s1c s2c s3sel
permutation s1d s2u s1c s2c s3sel
permutation s1x s2u s1c s2c s3sel
permutation s1p s2u s1c s2c s3sel
permutation s1n s2u s1a s2c s3sel
permutation s1n s2s s1c s2c s3sel
permutation s1d s2d s1c s2c s3sel
permutation s1n s2d s1c s2c s3sel
permutation s1n s2k s1c s2c s3sel
permutation s2ie s1c s2c
permutation s1n s2i s1c s2c s3sel
permutation s1d s2i s1c s2c s3sel
permutation s1x s2i s1c s2c s3sel
permutation s1n s2j s1c s2c s3sel
permutation s1d s2x s1c s2c s3sel
permutation s1n s2x s1a s2c s3sel

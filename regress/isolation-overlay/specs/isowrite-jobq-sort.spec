# isowrite D3277 JOBQ SORT: the job-queue claim whose subquery sorts (no index on the ORDER BY: InitPlan 1 is Limit ->
# LockRows -> Sort -> Seq Scan), UPDATE / DELETE job .. WHERE id = (SELECT id FROM job WHERE state = 'ready' .. ORDER BY
# prio, id LIMIT 1 FOR UPDATE [SKIP LOCKED]). The InitPlan's LockRows keeps its own row mark and rechecks under its own
# EvalPlanQual over the sorted rows: a row another transaction holds is skipped (SKIP LOCKED: the next row in sort order)
# or waited for (FOR UPDATE); after the wait the newest version is rechecked against the subquery's quals, a row that no
# longer qualifies (moved out of 'ready', deleted) is passed over and the Limit takes the next row of the Sort, and one
# that still qualifies is claimed in its newest version, even when its sort key moved (the Sort is not redone). The
# write's own recheck copies the InitPlan's value. The head row is id 5 (prio 0, 'ready', 'mail').

setup
{
 CREATE TABLE job (id int PRIMARY KEY, queue text NOT NULL, prio int NOT NULL, state text NOT NULL, attempts int NOT NULL DEFAULT 0, note text);
 INSERT INTO job SELECT i, CASE WHEN i % 3 = 0 THEN 'sms' ELSE 'mail' END, i % 5, CASE WHEN i % 4 = 0 THEN 'done' ELSE 'ready' END, 0, 'n' || i FROM generate_series(1, 200) i;
 ANALYZE job;
}

teardown
{
 DROP TABLE job;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1n	{ UPDATE job SET note = 'held', attempts = attempts + 10 WHERE id = 5; }
step s1l	{ SELECT id FROM job WHERE id = 5 FOR UPDATE; }
step s1d	{ UPDATE job SET state = 'done' WHERE id = 5; }
step s1x	{ DELETE FROM job WHERE id = 5; }
step s1p	{ UPDATE job SET prio = 4 WHERE id = 5; }
step s1c	{ COMMIT; }
step s1a	{ ROLLBACK; }

session s2
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s2e	{ EXPLAIN (COSTS OFF) UPDATE job SET state = 'running', attempts = attempts + 1 WHERE id = (SELECT id FROM job WHERE state = 'ready' AND queue = 'mail' ORDER BY prio, id LIMIT 1 FOR UPDATE SKIP LOCKED) RETURNING id; }
step s2u	{ UPDATE job SET state = 'running', attempts = attempts + 1 WHERE id = (SELECT id FROM job WHERE state = 'ready' AND queue = 'mail' ORDER BY prio, id LIMIT 1 FOR UPDATE) RETURNING id, queue, prio, attempts, note; }
step s2s	{ UPDATE job SET state = 'running', attempts = attempts + 1 WHERE id = (SELECT id FROM job WHERE state = 'ready' AND queue = 'mail' ORDER BY prio, id LIMIT 1 FOR UPDATE SKIP LOCKED) RETURNING id, queue, prio, attempts, note; }
step s2o	{ UPDATE job SET state = 'running' WHERE id = (SELECT id FROM job WHERE state = 'ready' AND queue = 'mail' ORDER BY prio, id LIMIT 1 OFFSET 1 FOR NO KEY UPDATE SKIP LOCKED) RETURNING id, prio; }
step s2d	{ DELETE FROM job WHERE id = (SELECT id FROM job WHERE state = 'ready' AND queue = 'mail' ORDER BY prio, id LIMIT 1 FOR UPDATE) RETURNING id, note; }
step s2k	{ DELETE FROM job WHERE id = (SELECT id FROM job WHERE state = 'ready' AND queue = 'mail' ORDER BY prio, id LIMIT 1 FOR UPDATE SKIP LOCKED) RETURNING id, note; }
step s2c	{ COMMIT; }

session s3
step s3sel	{ SELECT id, prio, state, attempts, note FROM job WHERE id IN (5, 10, 15, 20, 25) OR state = 'running' ORDER BY id; }

permutation s2e s1c s2c
permutation s1l s2s s1c s2c s3sel
permutation s1n s2s s1c s2c s3sel
permutation s1l s2o s1c s2c s3sel
permutation s1n s2u s1c s2c s3sel
permutation s1d s2u s1c s2c s3sel
permutation s1x s2u s1c s2c s3sel
permutation s1p s2u s1c s2c s3sel
permutation s1n s2u s1a s2c s3sel
permutation s1d s2d s1c s2c s3sel
permutation s1n s2d s1c s2c s3sel
permutation s1l s2k s1c s2c s3sel

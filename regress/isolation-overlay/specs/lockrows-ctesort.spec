# lockrows CTESORT (lk-0057 / lk-0065): the claim CTE whose lock sits over a Sort (Limit -> LockRows -> Incremental Sort ->
# Index Scan), its store read whole by the statement's UPDATE (a Nested Loop over the CTE Scan). The CTE's LockRows keeps
# its own row mark and rechecks under its own EvalPlanQual (in the IR over the Sort's producer): a row another
# transaction holds is skipped (SKIP LOCKED) or waited for (FOR UPDATE); after the wait the newest version is rechecked
# against the CTE's quals, a row that no longer qualifies (moved out of 'ready', deleted) is passed over and the Limit
# takes the next one in sort order, and one that still qualifies is claimed in its newest version.

setup
{
 CREATE TABLE job (id int PRIMARY KEY, queue text NOT NULL, prio int NOT NULL, state text NOT NULL, run_at timestamptz NOT NULL, note text);
 CREATE INDEX job_run_at ON job (run_at);
 INSERT INTO job SELECT i, CASE WHEN i % 3 = 0 THEN 'sms' ELSE 'mail' END, i % 5, CASE WHEN i % 4 = 0 THEN 'done' ELSE 'ready' END, timestamptz '2024-01-01 00:00+00' + ((i * 37) % 1000) * interval '1 minute', 'n' || i FROM generate_series(1, 200) i;
 ANALYZE job;
}

teardown
{
 DROP TABLE job;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1n	{ UPDATE job SET note = 'held' WHERE id = 109; }
step s1d	{ UPDATE job SET state = 'done' WHERE id = 109; }
step s1x	{ DELETE FROM job WHERE id = 109; }
step s1c	{ COMMIT; }
step s1a	{ ROLLBACK; }

session s2
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; SET enable_seqscan = off; }
step s2e	{ EXPLAIN (COSTS OFF) WITH c AS (SELECT id FROM job WHERE state = 'ready' AND queue = 'mail' ORDER BY run_at, id LIMIT 3 FOR UPDATE) UPDATE job j SET state = 'running' FROM c WHERE j.id = c.id; }
step s2u	{ WITH c AS (SELECT id FROM job WHERE state = 'ready' AND queue = 'mail' ORDER BY run_at, id LIMIT 3 FOR UPDATE) UPDATE job j SET state = 'running' FROM c WHERE j.id = c.id; }
step s2k	{ WITH c AS (SELECT id FROM job WHERE state = 'ready' AND queue = 'mail' ORDER BY run_at, id LIMIT 3 FOR UPDATE SKIP LOCKED) UPDATE job j SET state = 'running' FROM c WHERE j.id = c.id; }
step s2c	{ COMMIT; }

session s3
step s3sel	{ SELECT id, state, note FROM job WHERE state = 'running' OR id = 109 ORDER BY id; }

permutation s2e s1c s2c
permutation s1n s2u s1c s2c s3sel
permutation s1d s2u s1c s2c s3sel
permutation s1x s2u s1c s2c s3sel
permutation s1n s2u s1a s2c s3sel
permutation s1n s2k s1c s2c s3sel

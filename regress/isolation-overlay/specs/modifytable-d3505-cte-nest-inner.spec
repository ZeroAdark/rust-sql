# D3505: graphile-worker (<= 0.13) complete_job, a writing CTE read on the nest inner of the statement's own UPDATE:
# `WITH j AS (DELETE FROM gw_jobs WHERE id = .. AND locked_by = 'w1' RETURNING *) UPDATE gw_job_queues .. FROM j WHERE
# gw_job_queues.queue_name = j.queue_name AND gw_job_queues.locked_by = 'w1'`: Update over a Nested Loop, the queue
# scan (its filter keeps one row of twenty) the outer and the CTE Scan the inner. s1 holds the queue row q3 (or the job
# row 5). The UPDATE waits, then rechecks the queue row's newest version joined to the CTE's row from its COPY mark,
# never running the CTE again (PostgreSQL's EvalPlanQual): a version that still qualifies is updated (job_count kept),
# one locked by another worker or deleted is not, and the job row is deleted either way. When s1 holds the job row, the
# CTE's DELETE waits and rechecks it: a job taken by another worker is not deleted and the queue is left alone.
# complete_jobs (s2b) joins two jobs of q3 to the one queue row, which is updated once. fail_job's form (s2f) sets the
# queue row from the CTE's row.

setup
{
 CREATE TABLE gw_job_queues (queue_name text NOT NULL PRIMARY KEY, job_count int NOT NULL, locked_at timestamptz, locked_by text);
 CREATE TABLE gw_jobs (id bigint PRIMARY KEY, queue_name text, task_identifier text NOT NULL, attempts int DEFAULT 0 NOT NULL, run_at timestamptz NOT NULL, locked_at timestamptz, locked_by text, last_error text);
 INSERT INTO gw_job_queues SELECT 'q' || g, 10, CASE WHEN g IN (1, 3) THEN timestamptz '2026-01-01 00:00+00' END, CASE g WHEN 3 THEN 'w1' WHEN 1 THEN 'w9' END FROM generate_series(1, 20) g;
 INSERT INTO gw_jobs SELECT g, CASE WHEN g % 4 = 0 THEN NULL ELSE 'q' || (g % 3 + 1) END, CASE WHEN g % 2 = 0 THEN 'send_email' ELSE 'resize' END, g % 3, timestamptz '2026-01-01 00:00+00' + g * interval '1 second', CASE WHEN g IN (5, 9, 11) THEN timestamptz '2026-01-01 00:00+00' END, CASE WHEN g IN (5, 9, 11) THEN 'w1' END FROM generate_series(1, 40) g;
 ANALYZE gw_jobs; ANALYZE gw_job_queues;
}

teardown
{
 DROP TABLE gw_jobs, gw_job_queues;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1q	{ UPDATE gw_job_queues SET job_count = job_count + 100 WHERE queue_name = 'q3'; }
step s1l	{ UPDATE gw_job_queues SET locked_by = 'w2' WHERE queue_name = 'q3'; }
step s1d	{ DELETE FROM gw_job_queues WHERE queue_name = 'q3'; }
step s1j	{ UPDATE gw_jobs SET locked_by = 'w2' WHERE id = 5; }
step s1c	{ COMMIT; }
step s1a	{ ROLLBACK; }

session s2
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s2e	{ EXPLAIN (COSTS OFF) WITH j AS (DELETE FROM gw_jobs WHERE id = 5 AND locked_by = 'w1' RETURNING *) UPDATE gw_job_queues SET locked_by = NULL, locked_at = NULL, job_count = gw_job_queues.job_count - 1 FROM j WHERE gw_job_queues.queue_name = j.queue_name AND gw_job_queues.locked_by = 'w1'; }
step s2c1	{ WITH j AS (DELETE FROM gw_jobs WHERE id = 5 AND locked_by = 'w1' RETURNING *) UPDATE gw_job_queues SET locked_by = NULL, locked_at = NULL, job_count = gw_job_queues.job_count - 1 FROM j WHERE gw_job_queues.queue_name = j.queue_name AND gw_job_queues.locked_by = 'w1' RETURNING gw_job_queues.queue_name, gw_job_queues.job_count, gw_job_queues.locked_by; }
step s2f	{ WITH j AS (UPDATE gw_jobs SET attempts = attempts + 1, last_error = 'boom', locked_by = NULL, locked_at = NULL WHERE id = 5 AND locked_by = 'w1' RETURNING *) UPDATE gw_job_queues SET locked_by = j.task_identifier || ':' || j.attempts, locked_at = NULL FROM j WHERE gw_job_queues.queue_name = j.queue_name AND gw_job_queues.locked_by = 'w1' RETURNING gw_job_queues.queue_name, gw_job_queues.locked_by; }
step s2b	{ WITH j AS (DELETE FROM gw_jobs WHERE id = ANY (ARRAY[5, 9, 11]::bigint[]) AND locked_by = 'w1' RETURNING *) UPDATE gw_job_queues SET locked_by = NULL, locked_at = NULL, job_count = gw_job_queues.job_count - 1 FROM j WHERE gw_job_queues.queue_name = j.queue_name AND gw_job_queues.locked_by = 'w1'; }
step s2c	{ COMMIT; }

session s3
step s3sel	{ SELECT * FROM gw_job_queues WHERE queue_name IN ('q1', 'q3') ORDER BY 1; SELECT id, queue_name, attempts, locked_by, last_error FROM gw_jobs WHERE id IN (5, 9, 11) ORDER BY id; }

permutation s2e s2c
permutation s1q s2c1 s1c s2c s3sel
permutation s1l s2c1 s1c s2c s3sel
permutation s1d s2c1 s1c s2c s3sel
permutation s1q s2c1 s1a s2c s3sel
permutation s1j s2c1 s1c s2c s3sel
permutation s1q s2f s1c s2c s3sel
permutation s1l s2f s1c s2c s3sel
permutation s1q s2b s1c s2c s3sel
permutation s1l s2b s1c s2c s3sel

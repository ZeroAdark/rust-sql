# D3561 REFANTI: procrastinate's fetch_job, the claim CTE (NOT EXISTS over earlier jobs of the same lock, ORDER BY priority
# DESC, id LIMIT 1 FOR UPDATE OF jobs) beside the UPDATE that marks the claimed job 'doing'. Over a newer version of the
# claimed job (READ COMMITTED; the waiting form: SKIP LOCKED skips a locked job, no recheck) PostgreSQL's EvalPlanQual
# gives the earlier jobs no test tuple (their junk ctid NULL above the anti join), so the newest version passes the anti
# join on its own quals alone: a job moved into a lock that an earlier job holds is still claimed; a job no longer 'todo'
# or scheduled later is skipped and the Limit takes the next one. The UPDATE's recheck reads the CTE's stored row. s2:
# the claim in the priority index's order; s3: under a Sort over a Hash Anti Join (no index scan). Expected output is
# PostgreSQL 18.6's.
setup
{
  CREATE TYPE pj_status AS ENUM ('todo', 'doing', 'succeeded', 'failed');
  CREATE TABLE pj (id bigint PRIMARY KEY, queue_name varchar(128) NOT NULL, priority int NOT NULL DEFAULT 0, lock text, status pj_status NOT NULL DEFAULT 'todo', scheduled_at timestamptz, attempts int NOT NULL DEFAULT 0, worker_id bigint);
  CREATE UNIQUE INDEX pj_lock_idx ON pj (lock) WHERE status = 'doing';
  CREATE INDEX pj_id_lock_idx ON pj (id, lock) WHERE status = ANY (ARRAY['todo'::pj_status, 'doing'::pj_status]);
  CREATE INDEX pj_priority_idx ON pj (priority DESC, id ASC) WHERE status = 'todo'::pj_status;
  INSERT INTO pj (id, queue_name, priority, lock, status, scheduled_at) SELECT g, CASE WHEN g % 3 = 0 THEN 'mail' ELSE 'default' END, g % 3, CASE WHEN g % 4 = 0 THEN 'lock' || (g % 5) END, (CASE WHEN g % 10 = 0 THEN 'succeeded' WHEN g = 8 THEN 'doing' ELSE 'todo' END)::pj_status, CASE WHEN g % 7 = 0 THEN timestamptz '2999-01-01' END FROM generate_series(1, 300) g;
  ANALYZE pj;
}
teardown { DROP TABLE pj; DROP TYPE pj_status; }

session s1
setup		{ BEGIN; }
step s1a	{ UPDATE pj SET attempts = attempts + 1 WHERE id = 2; }
step s1e	{ UPDATE pj SET lock = 'lockA' WHERE id IN (1, 2); }
step s1s	{ UPDATE pj SET status = 'doing', worker_id = 9 WHERE id = 2; }
step s1p	{ UPDATE pj SET scheduled_at = timestamptz '2999-01-01' WHERE id = 2; }
step s1k	{ SELECT id FROM pj WHERE id = 2 FOR UPDATE; }
step s1c	{ COMMIT; }
teardown	{ ROLLBACK; }

session s2
setup		{ BEGIN; }
step s2x	{ EXPLAIN (COSTS OFF) WITH candidate AS (SELECT jobs.* FROM pj AS jobs WHERE NOT EXISTS (SELECT 1 FROM pj AS earlier_jobs WHERE jobs.lock IS NOT NULL AND earlier_jobs.lock = jobs.lock AND earlier_jobs.status IN ('todo', 'doing') AND earlier_jobs.id < jobs.id) AND jobs.status = 'todo' AND (jobs.scheduled_at IS NULL OR jobs.scheduled_at <= now()) ORDER BY jobs.priority DESC, jobs.id ASC LIMIT 1 FOR UPDATE OF jobs) UPDATE pj SET status = 'doing', worker_id = 7 FROM candidate WHERE pj.id = candidate.id RETURNING pj.id, pj.lock, pj.attempts, pj.status, pj.worker_id; }
step s2f	{ WITH candidate AS (SELECT jobs.* FROM pj AS jobs WHERE NOT EXISTS (SELECT 1 FROM pj AS earlier_jobs WHERE jobs.lock IS NOT NULL AND earlier_jobs.lock = jobs.lock AND earlier_jobs.status IN ('todo', 'doing') AND earlier_jobs.id < jobs.id) AND jobs.status = 'todo' AND (jobs.scheduled_at IS NULL OR jobs.scheduled_at <= now()) ORDER BY jobs.priority DESC, jobs.id ASC LIMIT 1 FOR UPDATE OF jobs) UPDATE pj SET status = 'doing', worker_id = 7 FROM candidate WHERE pj.id = candidate.id RETURNING pj.id, pj.lock, pj.attempts, pj.status, pj.worker_id; }
step s2sk	{ WITH candidate AS (SELECT jobs.* FROM pj AS jobs WHERE NOT EXISTS (SELECT 1 FROM pj AS earlier_jobs WHERE jobs.lock IS NOT NULL AND earlier_jobs.lock = jobs.lock AND earlier_jobs.status IN ('todo', 'doing') AND earlier_jobs.id < jobs.id) AND jobs.status = 'todo' AND (jobs.scheduled_at IS NULL OR jobs.scheduled_at <= now()) ORDER BY jobs.priority DESC, jobs.id ASC LIMIT 1 FOR UPDATE OF jobs SKIP LOCKED) UPDATE pj SET status = 'doing', worker_id = 7 FROM candidate WHERE pj.id = candidate.id RETURNING pj.id, pj.lock, pj.status; }
step s2c	{ COMMIT; }

session s3
setup		{ SET enable_indexscan = off; SET enable_bitmapscan = off; BEGIN; }
step s3x	{ EXPLAIN (COSTS OFF) WITH candidate AS (SELECT jobs.* FROM pj AS jobs WHERE NOT EXISTS (SELECT 1 FROM pj AS earlier_jobs WHERE jobs.lock IS NOT NULL AND earlier_jobs.lock = jobs.lock AND earlier_jobs.status IN ('todo', 'doing') AND earlier_jobs.id < jobs.id) AND jobs.status = 'todo' AND (jobs.scheduled_at IS NULL OR jobs.scheduled_at <= now()) ORDER BY jobs.priority DESC, jobs.id ASC LIMIT 1 FOR UPDATE OF jobs) UPDATE pj SET status = 'doing', worker_id = 7 FROM candidate WHERE pj.id = candidate.id RETURNING pj.id, pj.lock, pj.attempts, pj.status, pj.worker_id; }
step s3f	{ WITH candidate AS (SELECT jobs.* FROM pj AS jobs WHERE NOT EXISTS (SELECT 1 FROM pj AS earlier_jobs WHERE jobs.lock IS NOT NULL AND earlier_jobs.lock = jobs.lock AND earlier_jobs.status IN ('todo', 'doing') AND earlier_jobs.id < jobs.id) AND jobs.status = 'todo' AND (jobs.scheduled_at IS NULL OR jobs.scheduled_at <= now()) ORDER BY jobs.priority DESC, jobs.id ASC LIMIT 1 FOR UPDATE OF jobs) UPDATE pj SET status = 'doing', worker_id = 7 FROM candidate WHERE pj.id = candidate.id RETURNING pj.id, pj.lock, pj.attempts, pj.status, pj.worker_id; }
step s3c	{ COMMIT; }

# the claim order (priority 2, then id): 2, 5, 8 (doing), 11, 14 (scheduled later), 17, 20 (succeeded), 23, ..
permutation s2x s1a s2f s1c s2c
permutation s1e s2f s1c s2c
permutation s1s s2f s1c s2c
permutation s1p s2f s1c s2c
permutation s1k s2sk s1c s2c
permutation s3x s1a s3f s1c s3c
permutation s1e s3f s1c s3c
permutation s1s s3f s1c s3c

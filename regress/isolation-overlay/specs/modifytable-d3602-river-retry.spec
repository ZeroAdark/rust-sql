# D3602: River's JobRetry / JobCancel (a locking CTE, a data-modifying CTE's UPDATE over a Nested Loop whose outer is
# that CTE's CTE Scan, COPY-marked) against a concurrent change of the job (READ COMMITTED). The lock waits, then locks
# the newest version; the UPDATE finds the version its snapshot read updated and rechecks the newest one under
# EvalPlanQual, the CTE Scan handing on its stored row: a job that started running meanwhile is not retried (the
# statement then reads its snapshot version), one rescheduled meanwhile is, a deleted one is gone; a cancel of a job
# that started running keeps it running. The expected output is PostgreSQL 18.6's.
setup
{
  CREATE TYPE rr_state AS ENUM ('available', 'cancelled', 'completed', 'discarded', 'running', 'scheduled');
  CREATE TABLE rr_job (id bigint PRIMARY KEY, state rr_state NOT NULL, attempt int NOT NULL DEFAULT 1, max_attempts int NOT NULL DEFAULT 1, finalized_at timestamptz, scheduled_at timestamptz NOT NULL DEFAULT '2026-01-01', queue text NOT NULL DEFAULT 'default');
  INSERT INTO rr_job (id, state, finalized_at) VALUES (1, 'discarded', '2026-01-02'), (2, 'scheduled', NULL), (3, 'discarded', '2026-01-02');
  INSERT INTO rr_job (id, state) SELECT g, 'available' FROM generate_series(10, 600) g;
  ANALYZE rr_job;
}
teardown { DROP TABLE rr_job; DROP TYPE rr_state; }

session s1
setup		{ BEGIN; }
step s1run	{ UPDATE rr_job SET state = 'running', finalized_at = NULL WHERE id = 1; }
step s1sch	{ UPDATE rr_job SET scheduled_at = '2026-02-01' WHERE id = 1; }
step s1del	{ DELETE FROM rr_job WHERE id = 1; }
step s1run2	{ UPDATE rr_job SET state = 'running' WHERE id = 2; }
step s1c	{ COMMIT; }

session s2
step s2x	{ EXPLAIN (COSTS OFF) WITH job_to_update AS (SELECT id FROM rr_job WHERE rr_job.id = 1 FOR UPDATE), updated_job AS (UPDATE rr_job SET state = 'available', scheduled_at = '2026-03-01', max_attempts = CASE WHEN attempt = max_attempts THEN max_attempts + 1 ELSE max_attempts END, finalized_at = NULL FROM job_to_update WHERE rr_job.id = job_to_update.id AND rr_job.state != 'running' RETURNING rr_job.*) SELECT id, state FROM updated_job UNION ALL SELECT id, state FROM rr_job WHERE id = 1 AND id NOT IN (SELECT id FROM updated_job); }
step s2retry	{ WITH job_to_update AS (SELECT id FROM rr_job WHERE rr_job.id = 1 FOR UPDATE), updated_job AS (UPDATE rr_job SET state = 'available', scheduled_at = '2026-03-01', max_attempts = CASE WHEN attempt = max_attempts THEN max_attempts + 1 ELSE max_attempts END, finalized_at = NULL FROM job_to_update WHERE rr_job.id = job_to_update.id AND rr_job.state != 'running' RETURNING rr_job.*) SELECT id, state, scheduled_at, max_attempts, finalized_at FROM updated_job UNION ALL SELECT id, state, scheduled_at, max_attempts, finalized_at FROM rr_job WHERE id = 1 AND id NOT IN (SELECT id FROM updated_job); }
step s2cancel	{ WITH locked_job AS (SELECT id, queue, state, finalized_at FROM rr_job WHERE rr_job.id = 2 FOR UPDATE), notification AS (SELECT id, pg_notify('rr_control', json_build_object('action', 'cancel', 'job_id', id)::text) FROM locked_job WHERE state NOT IN ('cancelled', 'completed', 'discarded') AND finalized_at IS NULL), updated_job AS (UPDATE rr_job SET state = CASE WHEN state = 'running' THEN state ELSE 'cancelled' END, finalized_at = CASE WHEN state = 'running' THEN finalized_at ELSE '2026-03-01' END FROM notification WHERE rr_job.id = notification.id RETURNING rr_job.*) SELECT id, state, finalized_at FROM rr_job WHERE id = 2 AND id NOT IN (SELECT id FROM updated_job) UNION SELECT id, state, finalized_at FROM updated_job; }
step s2show	{ SELECT id, state, scheduled_at, finalized_at FROM rr_job WHERE id IN (1, 2) ORDER BY id; }

permutation s2x s1run s2retry s1c s2show
permutation s1sch s2retry s1c s2show
permutation s1del s2retry s1c s2show
permutation s1run2 s2cancel s1c s2show

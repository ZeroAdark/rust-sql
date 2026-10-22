# D3506: graphile-worker <= 0.13's get_job (a plpgsql function), its claim's filter probing a correlated EXISTS SubPlan
# whose plan locks the job's queue (`jobs.queue_name IS NULL OR EXISTS (SELECT 1 FROM job_queues WHERE
# job_queues.queue_name = jobs.queue_name .. FOR UPDATE SKIP LOCKED)`) under the claim's own LIMIT 1 FOR UPDATE SKIP
# LOCKED (READ COMMITTED). PostgreSQL runs the SubPlan per row in the claim's scan, its LockRows locking the row's queue
# (one another session holds skipped: the EXISTS fails); a claim that waits (FOR UPDATE, no SKIP LOCKED) on a job
# another session changed rechecks the newest version, running the EXISTS again with its queue: our own lock comes back
# TM_Ok, a queue still held is skipped, one released since is locked now. The scenarios: two workers at once; a held
# queue; an updated job; a job taken meanwhile; a job moved to a queue a third session holds; a job moved to a queue
# released before the recheck. Each prints whether each queue's xmax is this transaction's own xid and a fourth
# session's pgrowlocks of the queues (no multixact and no other lock mode for a row locked twice), and the jobs and
# queues claimed. Over these few jobs the planner sorts a Seq Scan (Limit -> LockRows -> Sort -> Seq Scan, the EXISTS
# in the scan's filter): the Sort runs every candidate's EXISTS, locking each available queue, and the claim's
# recheck reruns Sort -> scan over the newest version, its EXISTS locking again (lockrows-d3506-gw013-exists: the
# index form). The expected output is PostgreSQL 18.6's.
setup
{
  CREATE TABLE job_queues (queue_name text PRIMARY KEY, job_count int NOT NULL, locked_at timestamptz, locked_by text);
  CREATE TABLE jobs (id bigserial PRIMARY KEY, queue_name text, task_identifier text NOT NULL, payload json NOT NULL DEFAULT json_build_object(), priority int NOT NULL DEFAULT 0, run_at timestamptz NOT NULL DEFAULT '2026-01-01', attempts int NOT NULL DEFAULT 0, max_attempts int NOT NULL DEFAULT 25, last_error text, created_at timestamptz NOT NULL DEFAULT '2026-01-01', updated_at timestamptz NOT NULL DEFAULT '2026-01-01', key text UNIQUE, locked_at timestamptz, locked_by text, revision int NOT NULL DEFAULT 0, flags jsonb);
  CREATE INDEX jobs_priority_run_at_id_locked_at_without_failures_idx ON jobs (priority, run_at, id, locked_at) WHERE attempts < max_attempts;
  INSERT INTO job_queues VALUES ('q1', 1), ('q2', 1), ('q3', 0);
  INSERT INTO jobs (id, queue_name, task_identifier, priority, run_at) VALUES (1, 'q1', 't0', 0, '2026-01-01 00:01'), (2, 'q2', 't0', 0, '2026-01-01 00:02'), (3, NULL, 't1', 0, '2026-01-01 00:03'), (4, 'q1', 't1', 1, '2026-01-01 00:04');
  ANALYZE job_queues; ANALYZE jobs;
  CREATE EXTENSION pgrowlocks;
}
setup
{
  CREATE FUNCTION get_job(worker_id text, task_identifiers text[] = NULL, job_expiry interval = interval '4 hours', forbidden_flags text[] = NULL, now timestamptz = timestamptz '2026-01-02') RETURNS jobs AS $$ DECLARE v_job_id bigint; v_queue_name text; v_row jobs; BEGIN IF worker_id IS NULL OR length(worker_id) < 10 THEN RAISE EXCEPTION 'invalid worker id'; END IF; SELECT jobs.queue_name, jobs.id INTO v_queue_name, v_job_id FROM jobs WHERE (jobs.locked_at IS NULL OR jobs.locked_at < (now - job_expiry)) AND (jobs.queue_name IS NULL OR EXISTS (SELECT 1 FROM job_queues WHERE job_queues.queue_name = jobs.queue_name AND (job_queues.locked_at IS NULL OR job_queues.locked_at < (now - job_expiry)) FOR UPDATE SKIP LOCKED)) AND run_at <= now AND attempts < max_attempts AND (task_identifiers IS NULL OR task_identifier = any(task_identifiers)) AND (forbidden_flags IS NULL OR (flags ?| forbidden_flags) IS NOT TRUE) ORDER BY priority ASC, run_at ASC, id ASC LIMIT 1 FOR UPDATE SKIP LOCKED; IF v_job_id IS NULL THEN RETURN NULL; END IF; IF v_queue_name IS NOT NULL THEN UPDATE job_queues SET locked_by = worker_id, locked_at = now WHERE job_queues.queue_name = v_queue_name; END IF; UPDATE jobs SET attempts = attempts + 1, locked_by = worker_id, locked_at = now WHERE id = v_job_id RETURNING * INTO v_row; RETURN v_row; END; $$ LANGUAGE plpgsql VOLATILE;
}
teardown { DROP FUNCTION get_job; DROP TABLE jobs, job_queues; DROP EXTENSION pgrowlocks; }

session s1
setup		{ BEGIN; }
step s1get	{ SELECT id, queue_name, attempts, locked_by FROM get_job('worker-0000000001'); }
step s1holdA	{ SELECT queue_name FROM job_queues WHERE queue_name = 'q1' FOR UPDATE; }
step s1upd	{ UPDATE jobs SET payload = json_build_object('p', 1) WHERE id = 1; }
step s1take	{ UPDATE jobs SET locked_at = '2026-01-02', locked_by = 'other' WHERE id = 1; }
step s1toB	{ UPDATE jobs SET queue_name = 'q2' WHERE id = 1; }
step s1toC	{ UPDATE jobs SET queue_name = 'q3' WHERE id = 1; }
step s1c	{ COMMIT; }

session s2
step s2get	{ BEGIN; SELECT id, queue_name, attempts, locked_by FROM get_job('worker-0000000002'); }
step s2wait	{ BEGIN; SELECT jobs.queue_name, jobs.id FROM jobs WHERE (jobs.locked_at IS NULL OR jobs.locked_at < (timestamptz '2026-01-02' - interval '4 hours')) AND (jobs.queue_name IS NULL OR EXISTS (SELECT 1 FROM job_queues WHERE job_queues.queue_name = jobs.queue_name AND (job_queues.locked_at IS NULL OR job_queues.locked_at < (timestamptz '2026-01-02' - interval '4 hours')) FOR UPDATE SKIP LOCKED)) AND run_at <= timestamptz '2026-01-02' AND attempts < max_attempts ORDER BY priority ASC, run_at ASC, id ASC LIMIT 1 FOR UPDATE; }
step s2rows	{ SELECT queue_name, locked_by, xmax::text::bigint = txid_current() AS mine FROM job_queues ORDER BY 1; }
step s2c	{ COMMIT; }

session s3
step s3holdB	{ BEGIN; SELECT queue_name FROM job_queues WHERE queue_name = 'q2' FOR UPDATE; }
step s3holdC	{ BEGIN; SELECT queue_name FROM job_queues WHERE queue_name = 'q3' FOR UPDATE; }
step s3c	{ COMMIT; }

session s4
step s4locks	{ SELECT locked_row, multi, modes FROM pgrowlocks('job_queues') ORDER BY locked_row; }
step s4show	{ SELECT 'job' AS t, id::text AS k, queue_name AS q, locked_by, attempts FROM jobs WHERE locked_by IS NOT NULL UNION ALL SELECT 'queue', queue_name, NULL, locked_by, NULL FROM job_queues WHERE locked_by IS NOT NULL ORDER BY 1, 2; }

permutation s1get s2get s2rows s4locks s1c s2c s4show
permutation s1holdA s2get s1c s2rows s4locks s2c s4show
permutation s1upd s2wait s1c s2rows s4locks s2c s4show
permutation s1take s2wait s1c s2rows s4locks s2c s4show
permutation s3holdC s1toC s2wait s1c s3c s2rows s4locks s2c s4show
permutation s3holdB s1toB s2wait s3c s1c s2rows s4locks s2c s4show

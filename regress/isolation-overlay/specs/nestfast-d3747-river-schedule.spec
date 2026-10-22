# D3747: River's JobSchedule as River runs it (a locking CTE of the jobs to schedule, a window CTE numbering each unique
# key's jobs, the conflicts with other jobs in the unique index's states as a merge join, the UPDATE of each job in a
# data-modifying CTE over its own scan through an INNER hash probe and a LEFT one) against a concurrent change of a job
# (READ COMMITTED). The lock waits, then locks the newest version; the UPDATE finds the version its snapshot read
# updated and rechecks the newest one under EvalPlanQual through both probes in the IR: a job with no conflict stays
# NULL-extended (made available), one with a conflict keeps it (discarded), a job that started running meanwhile is not
# locked, so not scheduled. About 200 jobs are due, so PostgreSQL 18.6 plans the hash form (with a handful it plans a
# nested loop over a Materialize of the conflicts: D3749). The expected output is PostgreSQL 18.6's.
setup
{
  CREATE TYPE rs_state AS ENUM ('available', 'cancelled', 'completed', 'discarded', 'pending', 'retryable', 'running', 'scheduled');
  CREATE TABLE rs_job (id bigint PRIMARY KEY, priority smallint NOT NULL DEFAULT 1, state rs_state NOT NULL, scheduled_at timestamptz NOT NULL DEFAULT '2026-01-01', finalized_at timestamptz, metadata jsonb NOT NULL DEFAULT jsonb_build_object(), unique_key bytea, unique_states bit(8));
  CREATE FUNCTION rs_in_bitmask(bitmask bit(8), state rs_state) RETURNS boolean LANGUAGE sql IMMUTABLE AS $$ SELECT CASE state WHEN 'available' THEN get_bit(bitmask, 7) WHEN 'cancelled' THEN get_bit(bitmask, 6) WHEN 'completed' THEN get_bit(bitmask, 5) WHEN 'discarded' THEN get_bit(bitmask, 4) WHEN 'pending' THEN get_bit(bitmask, 3) WHEN 'retryable' THEN get_bit(bitmask, 2) WHEN 'running' THEN get_bit(bitmask, 1) WHEN 'scheduled' THEN get_bit(bitmask, 0) ELSE 0 END = 1 $$;
  CREATE UNIQUE INDEX rs_job_unique_idx ON rs_job (unique_key) WHERE unique_key IS NOT NULL AND unique_states IS NOT NULL AND rs_in_bitmask(unique_states, state);
  INSERT INTO rs_job (id, state, unique_key, unique_states) VALUES (1, 'scheduled', '\x01', B'11110101'), (2, 'scheduled', '\x02', B'01110101'), (3, 'available', '\x02', B'01110101'), (4, 'scheduled', NULL, NULL), (5, 'retryable', '\x05', B'11110101');
  INSERT INTO rs_job (id, state, unique_key, unique_states) SELECT g, CASE WHEN g % 2 = 0 THEN 'scheduled' ELSE 'completed' END::rs_state, decode(lpad(to_hex(g), 4, '0'), 'hex'), B'11110101' FROM generate_series(10, 400) g;
  ANALYZE rs_job;
}
teardown { DROP TABLE rs_job; DROP FUNCTION rs_in_bitmask; DROP TYPE rs_state; }

session s1
setup		{ BEGIN; }
step s1prio1	{ UPDATE rs_job SET priority = 2 WHERE id = 1; }
step s1prio2	{ UPDATE rs_job SET priority = 2 WHERE id = 2; }
step s1run5	{ UPDATE rs_job SET state = 'running' WHERE id = 5; }
step s1c	{ COMMIT; }

session s2
step s2x	{ EXPLAIN (COSTS OFF) WITH jobs_to_schedule AS (SELECT id, unique_key, unique_states, priority, scheduled_at FROM rs_job WHERE state IN ('retryable', 'scheduled') AND scheduled_at <= '2026-01-02' ORDER BY priority, scheduled_at, id LIMIT 100 FOR UPDATE), jobs_with_rownum AS (SELECT *, CASE WHEN unique_key IS NOT NULL AND unique_states IS NOT NULL THEN ROW_NUMBER() OVER (PARTITION BY unique_key ORDER BY priority, scheduled_at, id) ELSE NULL END AS row_num FROM jobs_to_schedule), unique_conflicts AS (SELECT rs_job.unique_key FROM rs_job JOIN jobs_with_rownum ON rs_job.unique_key = jobs_with_rownum.unique_key AND rs_job.id != jobs_with_rownum.id WHERE rs_job.unique_key IS NOT NULL AND rs_job.unique_states IS NOT NULL AND rs_in_bitmask(rs_job.unique_states, rs_job.state)), job_updates AS (SELECT job.id, CASE WHEN job.row_num IS NULL THEN 'available'::rs_state WHEN uc.unique_key IS NOT NULL THEN 'discarded'::rs_state WHEN job.row_num = 1 THEN 'available'::rs_state ELSE 'discarded'::rs_state END AS new_state, (job.row_num IS NOT NULL AND (uc.unique_key IS NOT NULL OR job.row_num > 1)) AS finalized_at_do_update FROM jobs_with_rownum job LEFT JOIN unique_conflicts uc ON job.unique_key = uc.unique_key), updated_jobs AS (UPDATE rs_job SET state = job_updates.new_state, finalized_at = CASE WHEN job_updates.finalized_at_do_update THEN '2026-01-02' ELSE rs_job.finalized_at END, metadata = CASE WHEN job_updates.finalized_at_do_update THEN rs_job.metadata || jsonb_build_object('unique_key_conflict', 'scheduler_discarded') ELSE rs_job.metadata END FROM job_updates WHERE rs_job.id = job_updates.id RETURNING rs_job.id, job_updates.new_state = 'discarded'::rs_state AS conflict_discarded) SELECT rs_job.id, rs_job.state, updated_jobs.conflict_discarded FROM rs_job JOIN updated_jobs ON rs_job.id = updated_jobs.id; }
step s2sched	{ WITH jobs_to_schedule AS (SELECT id, unique_key, unique_states, priority, scheduled_at FROM rs_job WHERE state IN ('retryable', 'scheduled') AND scheduled_at <= '2026-01-02' ORDER BY priority, scheduled_at, id LIMIT 100 FOR UPDATE), jobs_with_rownum AS (SELECT *, CASE WHEN unique_key IS NOT NULL AND unique_states IS NOT NULL THEN ROW_NUMBER() OVER (PARTITION BY unique_key ORDER BY priority, scheduled_at, id) ELSE NULL END AS row_num FROM jobs_to_schedule), unique_conflicts AS (SELECT rs_job.unique_key FROM rs_job JOIN jobs_with_rownum ON rs_job.unique_key = jobs_with_rownum.unique_key AND rs_job.id != jobs_with_rownum.id WHERE rs_job.unique_key IS NOT NULL AND rs_job.unique_states IS NOT NULL AND rs_in_bitmask(rs_job.unique_states, rs_job.state)), job_updates AS (SELECT job.id, CASE WHEN job.row_num IS NULL THEN 'available'::rs_state WHEN uc.unique_key IS NOT NULL THEN 'discarded'::rs_state WHEN job.row_num = 1 THEN 'available'::rs_state ELSE 'discarded'::rs_state END AS new_state, (job.row_num IS NOT NULL AND (uc.unique_key IS NOT NULL OR job.row_num > 1)) AS finalized_at_do_update FROM jobs_with_rownum job LEFT JOIN unique_conflicts uc ON job.unique_key = uc.unique_key), updated_jobs AS (UPDATE rs_job SET state = job_updates.new_state, finalized_at = CASE WHEN job_updates.finalized_at_do_update THEN '2026-01-02' ELSE rs_job.finalized_at END, metadata = CASE WHEN job_updates.finalized_at_do_update THEN rs_job.metadata || jsonb_build_object('unique_key_conflict', 'scheduler_discarded') ELSE rs_job.metadata END FROM job_updates WHERE rs_job.id = job_updates.id RETURNING rs_job.id, job_updates.new_state = 'discarded'::rs_state AS conflict_discarded) SELECT rs_job.id, rs_job.state, updated_jobs.conflict_discarded FROM rs_job JOIN updated_jobs ON rs_job.id = updated_jobs.id WHERE rs_job.id < 10 ORDER BY rs_job.id; }
step s2show	{ SELECT id, priority, state, finalized_at, metadata FROM rs_job WHERE id < 10 ORDER BY id; }

permutation s2x s1prio1 s2sched s1c s2show
permutation s1prio2 s2sched s1c s2show
permutation s1run5 s2sched s1c s2show

# D3795 (with D3792 and D3749): River's JobSchedule at 5 jobs a run over a busier job table (about 1500 jobs due),
# against a concurrent change of a job (READ COMMITTED). With merge joins off (the scheduler's session) PostgreSQL 18.6
# takes each job by its primary key index scan, the inner of a Nested Loop whose outer is the Nested Loop Left Join of
# the window CTE's rows and a Materialize of the conflicts (a hash join of the unique index's bitmap scan and the window
# CTE's rows). The lock waits, then locks the newest version; the UPDATE finds the version its snapshot read updated and
# rechecks the newest one under EvalPlanQual in the IR: the job's index scan over the newest version, joined to the row
# the nests held. The expected output is PostgreSQL 18.6's.
setup
{
  CREATE TYPE rv_state AS ENUM ('available', 'cancelled', 'completed', 'discarded', 'pending', 'retryable', 'running', 'scheduled');
  CREATE TABLE rv_job (id bigint PRIMARY KEY, priority smallint NOT NULL DEFAULT 1, state rv_state NOT NULL, scheduled_at timestamptz NOT NULL DEFAULT '2026-01-01', finalized_at timestamptz, metadata jsonb NOT NULL DEFAULT jsonb_build_object(), unique_key bytea, unique_states bit(8));
  CREATE FUNCTION rv_in_bitmask(bitmask bit(8), state rv_state) RETURNS boolean LANGUAGE sql IMMUTABLE AS $$ SELECT CASE state WHEN 'available' THEN get_bit(bitmask, 7) WHEN 'cancelled' THEN get_bit(bitmask, 6) WHEN 'completed' THEN get_bit(bitmask, 5) WHEN 'discarded' THEN get_bit(bitmask, 4) WHEN 'pending' THEN get_bit(bitmask, 3) WHEN 'retryable' THEN get_bit(bitmask, 2) WHEN 'running' THEN get_bit(bitmask, 1) WHEN 'scheduled' THEN get_bit(bitmask, 0) ELSE 0 END = 1 $$;
  CREATE UNIQUE INDEX rv_job_unique_idx ON rv_job (unique_key) WHERE unique_key IS NOT NULL AND unique_states IS NOT NULL AND rv_in_bitmask(unique_states, state);
  INSERT INTO rv_job (id, state, unique_key, unique_states) VALUES (1, 'scheduled', '\x01', B'11110101'), (2, 'scheduled', '\x02', B'01110101'), (3, 'available', '\x02', B'01110101'), (4, 'scheduled', NULL, NULL), (5, 'retryable', '\x05', B'11110101');
  INSERT INTO rv_job (id, state, unique_key, unique_states) SELECT g, CASE WHEN g % 2 = 0 THEN 'scheduled' ELSE 'completed' END::rv_state, decode(lpad(to_hex(g), 4, '0'), 'hex'), B'11110101' FROM generate_series(10, 3000) g;
  ANALYZE rv_job;
}
teardown { DROP TABLE rv_job; DROP FUNCTION rv_in_bitmask; DROP TYPE rv_state; }

session s1
setup		{ BEGIN; }
step s1prio1	{ UPDATE rv_job SET priority = 2 WHERE id = 1; }
step s1prio2	{ UPDATE rv_job SET priority = 2 WHERE id = 2; }
step s1run5	{ UPDATE rv_job SET state = 'running' WHERE id = 5; }
step s1c	{ COMMIT; }

session s2
setup		{ SET enable_mergejoin = off; }
step s2x	{ EXPLAIN (COSTS OFF) WITH jobs_to_schedule AS (SELECT id, unique_key, unique_states, priority, scheduled_at FROM rv_job WHERE state IN ('retryable', 'scheduled') AND scheduled_at <= '2026-01-02' ORDER BY priority, scheduled_at, id LIMIT 5 FOR UPDATE), jobs_with_rownum AS (SELECT *, CASE WHEN unique_key IS NOT NULL AND unique_states IS NOT NULL THEN ROW_NUMBER() OVER (PARTITION BY unique_key ORDER BY priority, scheduled_at, id) ELSE NULL END AS row_num FROM jobs_to_schedule), unique_conflicts AS (SELECT rv_job.unique_key FROM rv_job JOIN jobs_with_rownum ON rv_job.unique_key = jobs_with_rownum.unique_key AND rv_job.id != jobs_with_rownum.id WHERE rv_job.unique_key IS NOT NULL AND rv_job.unique_states IS NOT NULL AND rv_in_bitmask(rv_job.unique_states, rv_job.state)), job_updates AS (SELECT job.id, CASE WHEN job.row_num IS NULL THEN 'available'::rv_state WHEN uc.unique_key IS NOT NULL THEN 'discarded'::rv_state WHEN job.row_num = 1 THEN 'available'::rv_state ELSE 'discarded'::rv_state END AS new_state, (job.row_num IS NOT NULL AND (uc.unique_key IS NOT NULL OR job.row_num > 1)) AS finalized_at_do_update FROM jobs_with_rownum job LEFT JOIN unique_conflicts uc ON job.unique_key = uc.unique_key), updated_jobs AS (UPDATE rv_job SET state = job_updates.new_state, finalized_at = CASE WHEN job_updates.finalized_at_do_update THEN '2026-01-02' ELSE rv_job.finalized_at END, metadata = CASE WHEN job_updates.finalized_at_do_update THEN rv_job.metadata || jsonb_build_object('unique_key_conflict', 'scheduler_discarded') ELSE rv_job.metadata END FROM job_updates WHERE rv_job.id = job_updates.id RETURNING rv_job.id, job_updates.new_state = 'discarded'::rv_state AS conflict_discarded) SELECT rv_job.id, rv_job.state, updated_jobs.conflict_discarded FROM rv_job JOIN updated_jobs ON rv_job.id = updated_jobs.id; }
step s2sched	{ WITH jobs_to_schedule AS (SELECT id, unique_key, unique_states, priority, scheduled_at FROM rv_job WHERE state IN ('retryable', 'scheduled') AND scheduled_at <= '2026-01-02' ORDER BY priority, scheduled_at, id LIMIT 5 FOR UPDATE), jobs_with_rownum AS (SELECT *, CASE WHEN unique_key IS NOT NULL AND unique_states IS NOT NULL THEN ROW_NUMBER() OVER (PARTITION BY unique_key ORDER BY priority, scheduled_at, id) ELSE NULL END AS row_num FROM jobs_to_schedule), unique_conflicts AS (SELECT rv_job.unique_key FROM rv_job JOIN jobs_with_rownum ON rv_job.unique_key = jobs_with_rownum.unique_key AND rv_job.id != jobs_with_rownum.id WHERE rv_job.unique_key IS NOT NULL AND rv_job.unique_states IS NOT NULL AND rv_in_bitmask(rv_job.unique_states, rv_job.state)), job_updates AS (SELECT job.id, CASE WHEN job.row_num IS NULL THEN 'available'::rv_state WHEN uc.unique_key IS NOT NULL THEN 'discarded'::rv_state WHEN job.row_num = 1 THEN 'available'::rv_state ELSE 'discarded'::rv_state END AS new_state, (job.row_num IS NOT NULL AND (uc.unique_key IS NOT NULL OR job.row_num > 1)) AS finalized_at_do_update FROM jobs_with_rownum job LEFT JOIN unique_conflicts uc ON job.unique_key = uc.unique_key), updated_jobs AS (UPDATE rv_job SET state = job_updates.new_state, finalized_at = CASE WHEN job_updates.finalized_at_do_update THEN '2026-01-02' ELSE rv_job.finalized_at END, metadata = CASE WHEN job_updates.finalized_at_do_update THEN rv_job.metadata || jsonb_build_object('unique_key_conflict', 'scheduler_discarded') ELSE rv_job.metadata END FROM job_updates WHERE rv_job.id = job_updates.id RETURNING rv_job.id, job_updates.new_state = 'discarded'::rv_state AS conflict_discarded) SELECT rv_job.id, rv_job.state, updated_jobs.conflict_discarded FROM rv_job JOIN updated_jobs ON rv_job.id = updated_jobs.id WHERE rv_job.id < 10 ORDER BY rv_job.id; }
step s2show	{ SELECT id, priority, state, finalized_at, metadata FROM rv_job WHERE id < 10 ORDER BY id; }

permutation s2x s1prio1 s2sched s1c s2show
permutation s1prio2 s2sched s1c s2show
permutation s1run5 s2sched s1c s2show

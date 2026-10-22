# D3807 (with D3749 and D3799): nestfast-d3800-river-hash-right after four rolled-back updates of every job left the job
# table's pages bloated (no autovacuum). PostgreSQL 18.6 takes each job by its primary key index scan, the inner of a
# Nested Loop whose outer is a Merge Left Join of the window CTE's rows and a Materialize of the conflicts (a Nested
# Loop of the window CTE's rows and the unique index's scan keyed by each). The lock waits, then locks the newest
# version; the UPDATE finds the version its snapshot read updated and rechecks the newest one under EvalPlanQual in the
# IR: the job's index scan over the newest version, joined to the row the merge held. The expected output is PostgreSQL
# 18.6's.
setup
{
  CREATE TYPE rb_state AS ENUM ('available', 'cancelled', 'completed', 'discarded', 'pending', 'retryable', 'running', 'scheduled');
  CREATE TABLE rb_job (id bigserial PRIMARY KEY, args jsonb NOT NULL DEFAULT jsonb_build_object(), attempt smallint NOT NULL DEFAULT 0, attempted_at timestamptz, attempted_by text[], created_at timestamptz NOT NULL DEFAULT '2026-01-01', errors jsonb[], finalized_at timestamptz, kind text NOT NULL, max_attempts smallint NOT NULL DEFAULT 25, metadata jsonb NOT NULL DEFAULT jsonb_build_object(), priority smallint NOT NULL DEFAULT 1, queue text NOT NULL DEFAULT 'default', state rb_state NOT NULL DEFAULT 'available', scheduled_at timestamptz NOT NULL DEFAULT '2026-01-01', tags varchar(255)[] NOT NULL DEFAULT ARRAY[]::varchar(255)[], unique_key bytea, unique_states bit(8)) WITH (autovacuum_enabled = false);
  CREATE FUNCTION rb_in_bitmask(bitmask bit(8), state rb_state) RETURNS boolean LANGUAGE sql IMMUTABLE AS $$ SELECT CASE state WHEN 'available' THEN get_bit(bitmask, 7) WHEN 'cancelled' THEN get_bit(bitmask, 6) WHEN 'completed' THEN get_bit(bitmask, 5) WHEN 'discarded' THEN get_bit(bitmask, 4) WHEN 'pending' THEN get_bit(bitmask, 3) WHEN 'retryable' THEN get_bit(bitmask, 2) WHEN 'running' THEN get_bit(bitmask, 1) WHEN 'scheduled' THEN get_bit(bitmask, 0) ELSE 0 END = 1 $$;
  CREATE INDEX rb_job_kind ON rb_job (kind);
  CREATE INDEX rb_state_and_finalized_at_index ON rb_job (state, finalized_at) WHERE finalized_at IS NOT NULL;
  CREATE INDEX rb_job_prioritized_fetching_index ON rb_job (state, queue, priority, scheduled_at, id);
  CREATE UNIQUE INDEX rb_job_unique_idx ON rb_job (unique_key) WHERE unique_key IS NOT NULL AND unique_states IS NOT NULL AND rb_in_bitmask(unique_states, state);
  INSERT INTO rb_job (kind, state, priority, scheduled_at, unique_key, unique_states, finalized_at)
  SELECT 'k' || (g % 7), (ARRAY['available', 'completed', 'scheduled', 'retryable', 'running', 'discarded'])[g % 6 + 1]::rb_state, 1 + g % 3, timestamptz '2026-01-01' + g * interval '1 minute',
    CASE WHEN g % 6 IN (0, 4) OR g % 5 = 0 THEN NULL ELSE decode(md5('u' || g), 'hex') END,
    CASE WHEN g % 6 IN (0, 4) OR g % 5 = 0 THEN NULL ELSE B'11110101' END,
    CASE WHEN g % 6 IN (1, 5) THEN timestamptz '2026-01-01' + g * interval '1 minute' END
  FROM generate_series(1, 3000) g;
  INSERT INTO rb_job (kind, state, priority, scheduled_at, unique_key, unique_states)
  SELECT 'dup', 'scheduled', 1 + g % 2, timestamptz '2026-01-01' + g * interval '1 minute', decode(md5('dup' || (g % 5)), 'hex'), B'00000001' FROM generate_series(1, 20) g;
  INSERT INTO rb_job (kind, state, priority, scheduled_at, unique_key, unique_states) VALUES ('dupa', 'available', 1, timestamptz '2025-12-01', decode(md5('dup0'), 'hex'), B'00000001');
  INSERT INTO rb_job (kind, state, priority, scheduled_at, unique_key, unique_states) VALUES ('dupc', 'scheduled', 1, timestamptz '2026-01-01 00:01', decode(md5('dup2'), 'hex'), B'00000001'), ('dupb', 'available', 1, timestamptz '2025-12-01', decode(md5('dup2'), 'hex'), B'11110101');
  ANALYZE rb_job;
}
setup { BEGIN; UPDATE rb_job SET attempt = attempt + 1; ROLLBACK; }
setup { BEGIN; UPDATE rb_job SET attempt = attempt + 1; ROLLBACK; }
setup { BEGIN; UPDATE rb_job SET attempt = attempt + 1; ROLLBACK; }
setup { BEGIN; UPDATE rb_job SET attempt = attempt + 1; ROLLBACK; }
teardown { DROP TABLE rb_job; DROP FUNCTION rb_in_bitmask; DROP TYPE rb_state; }

session s1
setup		{ BEGIN; }
step s1prioc	{ UPDATE rb_job SET priority = 2 WHERE kind = 'dupc'; }
step s1priod	{ UPDATE rb_job SET priority = 2 WHERE id = 3004; }
step s1run3	{ UPDATE rb_job SET state = 'running' WHERE id = 3; }
step s1c	{ COMMIT; }

session s2
step s2x	{ EXPLAIN (COSTS OFF) WITH jobs_to_schedule AS (SELECT id, unique_key, unique_states, priority, scheduled_at FROM rb_job WHERE state IN ('retryable', 'scheduled') AND scheduled_at <= '2026-01-02' ORDER BY priority, scheduled_at, id LIMIT 5 FOR UPDATE), jobs_with_rownum AS (SELECT *, CASE WHEN unique_key IS NOT NULL AND unique_states IS NOT NULL THEN ROW_NUMBER() OVER (PARTITION BY unique_key ORDER BY priority, scheduled_at, id) ELSE NULL END AS row_num FROM jobs_to_schedule), unique_conflicts AS (SELECT rb_job.unique_key FROM rb_job JOIN jobs_with_rownum ON rb_job.unique_key = jobs_with_rownum.unique_key AND rb_job.id != jobs_with_rownum.id WHERE rb_job.unique_key IS NOT NULL AND rb_job.unique_states IS NOT NULL AND rb_in_bitmask(rb_job.unique_states, rb_job.state)), job_updates AS (SELECT job.id, CASE WHEN job.row_num IS NULL THEN 'available'::rb_state WHEN uc.unique_key IS NOT NULL THEN 'discarded'::rb_state WHEN job.row_num = 1 THEN 'available'::rb_state ELSE 'discarded'::rb_state END AS new_state, (job.row_num IS NOT NULL AND (uc.unique_key IS NOT NULL OR job.row_num > 1)) AS finalized_at_do_update FROM jobs_with_rownum job LEFT JOIN unique_conflicts uc ON job.unique_key = uc.unique_key), updated_jobs AS (UPDATE rb_job SET state = job_updates.new_state, finalized_at = CASE WHEN job_updates.finalized_at_do_update THEN '2026-01-02' ELSE rb_job.finalized_at END, metadata = CASE WHEN job_updates.finalized_at_do_update THEN rb_job.metadata || jsonb_build_object('unique_key_conflict', 'scheduler_discarded') ELSE rb_job.metadata END FROM job_updates WHERE rb_job.id = job_updates.id RETURNING rb_job.id, job_updates.new_state = 'discarded'::rb_state AS conflict_discarded) SELECT rb_job.id, rb_job.state, updated_jobs.conflict_discarded FROM rb_job JOIN updated_jobs ON rb_job.id = updated_jobs.id; }
step s2sched	{ WITH jobs_to_schedule AS (SELECT id, unique_key, unique_states, priority, scheduled_at FROM rb_job WHERE state IN ('retryable', 'scheduled') AND scheduled_at <= '2026-01-02' ORDER BY priority, scheduled_at, id LIMIT 5 FOR UPDATE), jobs_with_rownum AS (SELECT *, CASE WHEN unique_key IS NOT NULL AND unique_states IS NOT NULL THEN ROW_NUMBER() OVER (PARTITION BY unique_key ORDER BY priority, scheduled_at, id) ELSE NULL END AS row_num FROM jobs_to_schedule), unique_conflicts AS (SELECT rb_job.unique_key FROM rb_job JOIN jobs_with_rownum ON rb_job.unique_key = jobs_with_rownum.unique_key AND rb_job.id != jobs_with_rownum.id WHERE rb_job.unique_key IS NOT NULL AND rb_job.unique_states IS NOT NULL AND rb_in_bitmask(rb_job.unique_states, rb_job.state)), job_updates AS (SELECT job.id, CASE WHEN job.row_num IS NULL THEN 'available'::rb_state WHEN uc.unique_key IS NOT NULL THEN 'discarded'::rb_state WHEN job.row_num = 1 THEN 'available'::rb_state ELSE 'discarded'::rb_state END AS new_state, (job.row_num IS NOT NULL AND (uc.unique_key IS NOT NULL OR job.row_num > 1)) AS finalized_at_do_update FROM jobs_with_rownum job LEFT JOIN unique_conflicts uc ON job.unique_key = uc.unique_key), updated_jobs AS (UPDATE rb_job SET state = job_updates.new_state, finalized_at = CASE WHEN job_updates.finalized_at_do_update THEN '2026-01-02' ELSE rb_job.finalized_at END, metadata = CASE WHEN job_updates.finalized_at_do_update THEN rb_job.metadata || jsonb_build_object('unique_key_conflict', 'scheduler_discarded') ELSE rb_job.metadata END FROM job_updates WHERE rb_job.id = job_updates.id RETURNING rb_job.id, job_updates.new_state = 'discarded'::rb_state AS conflict_discarded) SELECT rb_job.id, rb_job.state, updated_jobs.conflict_discarded FROM rb_job JOIN updated_jobs ON rb_job.id = updated_jobs.id ORDER BY rb_job.id; }
step s2show	{ SELECT id, kind, priority, state, finalized_at, metadata FROM rb_job WHERE id IN (3, 3002, 3004, 3006, 3022, 3023) ORDER BY id; }

permutation s2x s1prioc s2sched s1c s2show
permutation s1priod s2sched s1c s2show
permutation s1run3 s2sched s1c s2show

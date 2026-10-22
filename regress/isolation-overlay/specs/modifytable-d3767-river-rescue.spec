# D3767: River's JobRescueMany (UPDATE river_job .. FROM (SELECT unnest(..) AS id, ..) AS updated_job WHERE
# river_job.id = updated_job.id) over 20000 jobs, a batch of three: PostgreSQL 18.6 plans Update -> Nested Loop
# (Subquery Scan of the unnests over a ProjectSet, COPY-marked; Index Scan of river_job by its primary key). Against a
# concurrent change of a rescued job (READ COMMITTED) the UPDATE finds the version its snapshot read updated and
# rechecks the newest one under EvalPlanQual, the Subquery Scan handed its stored row: a job that finished meanwhile
# is rescued over its newest version (its state and errors overwritten and appended to), one whose errors grew keeps
# both errors, a deleted one is gone and one whose id moved is skipped. The expected output is PostgreSQL 18.6's.
setup
{
  CREATE TYPE river_job_state AS ENUM ('available', 'cancelled', 'completed', 'discarded', 'pending', 'retryable', 'running', 'scheduled');
  CREATE TABLE river_job (id bigserial PRIMARY KEY, state river_job_state NOT NULL DEFAULT 'available', attempt smallint NOT NULL DEFAULT 0, max_attempts smallint NOT NULL DEFAULT 25, attempted_at timestamptz, created_at timestamptz NOT NULL DEFAULT '2026-01-01', finalized_at timestamptz, scheduled_at timestamptz NOT NULL DEFAULT '2026-01-01', priority smallint NOT NULL DEFAULT 1, args jsonb NOT NULL DEFAULT jsonb_build_object(), attempted_by text[], errors jsonb[], kind text NOT NULL, metadata jsonb NOT NULL DEFAULT jsonb_build_object(), queue text NOT NULL DEFAULT 'default');
  CREATE INDEX river_job_prioritized_fetching_index ON river_job (state, queue, priority, scheduled_at, id);
  INSERT INTO river_job (state, attempt, attempted_at, finalized_at, scheduled_at, priority, kind, queue) SELECT (ARRAY['available', 'scheduled', 'retryable', 'running', 'completed', 'scheduled', 'discarded', 'cancelled'])[g % 8 + 1]::river_job_state, g % 3, CASE WHEN g % 8 = 3 THEN timestamptz '2026-01-01' + g * interval '1 second' END, CASE WHEN g % 8 IN (4, 6, 7) THEN timestamptz '2026-01-01' + g * interval '1 minute' END, timestamptz '2026-01-01' + (g % 50) * interval '1 hour', g % 4 + 1, 'k' || (g % 3), CASE WHEN g % 2 = 0 THEN 'default' ELSE 'mail' END FROM generate_series(1, 20000) g;
  ANALYZE river_job;
}
teardown { DROP TABLE river_job; DROP TYPE river_job_state; }

session s1
setup		{ BEGIN; }
step s1fin	{ UPDATE river_job SET state = 'completed', finalized_at = '2026-01-30' WHERE id = 11; }
step s1err	{ UPDATE river_job SET errors = array_append(errors, jsonb_build_object('error', 's1')) WHERE id = 11; }
step s1del	{ DELETE FROM river_job WHERE id = 11; }
step s1move	{ UPDATE river_job SET id = 99999 WHERE id = 11; }
step s1c	{ COMMIT; }

session s2
step s2x	{ EXPLAIN (COSTS OFF) UPDATE river_job SET errors = array_append(errors, updated_job.error), finalized_at = updated_job.finalized_at, scheduled_at = updated_job.scheduled_at, state = updated_job.state FROM (SELECT unnest(ARRAY[3, 11, 19]::bigint[]) AS id, unnest(ARRAY[jsonb_build_object('at', '2026-02-01', 'error', 'stuck'), jsonb_build_object('at', '2026-02-02', 'error', 'stuck'), jsonb_build_object('at', '2026-02-03', 'error', 'stuck')]::jsonb[]) AS error, nullif(unnest(ARRAY['0001-01-01 00:00:00+00', '2026-02-01', '0001-01-01 00:00:00+00']::timestamptz[]), '0001-01-01 00:00:00+00') AS finalized_at, unnest(ARRAY['2026-03-01', '2026-03-02', '2026-03-03']::timestamptz[]) AS scheduled_at, unnest(ARRAY['retryable', 'discarded', 'retryable']::text[])::river_job_state AS state) AS updated_job WHERE river_job.id = updated_job.id; }
step s2r	{ UPDATE river_job SET errors = array_append(errors, updated_job.error), finalized_at = updated_job.finalized_at, scheduled_at = updated_job.scheduled_at, state = updated_job.state FROM (SELECT unnest(ARRAY[3, 11, 19]::bigint[]) AS id, unnest(ARRAY[jsonb_build_object('at', '2026-02-01', 'error', 'stuck'), jsonb_build_object('at', '2026-02-02', 'error', 'stuck'), jsonb_build_object('at', '2026-02-03', 'error', 'stuck')]::jsonb[]) AS error, nullif(unnest(ARRAY['0001-01-01 00:00:00+00', '2026-02-01', '0001-01-01 00:00:00+00']::timestamptz[]), '0001-01-01 00:00:00+00') AS finalized_at, unnest(ARRAY['2026-03-01', '2026-03-02', '2026-03-03']::timestamptz[]) AS scheduled_at, unnest(ARRAY['retryable', 'discarded', 'retryable']::text[])::river_job_state AS state) AS updated_job WHERE river_job.id = updated_job.id; }
step s2show	{ SELECT id, state, finalized_at, scheduled_at, errors FROM river_job WHERE id IN (3, 11, 19, 99999) ORDER BY id; }

permutation s2x s1fin s2r s1c s2show
permutation s1err s2r s1c s2show
permutation s1del s2r s1c s2show
permutation s1move s2r s1c s2show

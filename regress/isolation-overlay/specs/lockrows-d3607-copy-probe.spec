# D3607: River <= 0.10's completer (a row lock inside a CTE over a hash join whose build is a CTE of unnest columns,
# COPY-marked; a data-modifying CTE's UPDATE over the lock's store) against a concurrent change of a job (READ
# COMMITTED). The lock waits, then rechecks the newest version under EvalPlanQual joined to the stored CTE row: a job
# another completer finished meanwhile (no longer running) is skipped and returned as the statement's snapshot read it,
# a running job whose metadata changed meanwhile is completed, a deleted one is skipped. The expected output is
# PostgreSQL 18.6's.
setup
{
  CREATE TABLE cpp_job (id bigint PRIMARY KEY, state text NOT NULL, finalized_at timestamptz, metadata jsonb NOT NULL DEFAULT jsonb_build_object());
  INSERT INTO cpp_job SELECT g, CASE WHEN g % 3 = 0 THEN 'available' ELSE 'running' END, NULL FROM generate_series(1, 400) g;
  ANALYZE cpp_job;
}
teardown { DROP TABLE cpp_job; }

session s1
setup		{ BEGIN; }
step s1fin	{ UPDATE cpp_job SET state = 'completed', finalized_at = '2026-01-05' WHERE id = 1; }
step s1meta	{ UPDATE cpp_job SET metadata = jsonb_build_object('seen', true) WHERE id = 1; }
step s1del	{ DELETE FROM cpp_job WHERE id = 2; }
step s1c	{ COMMIT; }

session s2
step s2x	{ EXPLAIN (COSTS OFF) WITH job_to_finalized_at AS (SELECT unnest(ARRAY[1, 2, 4]::bigint[]) AS id, unnest(ARRAY['2026-02-01', '2026-02-02', '2026-02-04']::timestamptz[]) AS finalized_at), job_to_update AS (SELECT cpp_job.id, job_to_finalized_at.finalized_at FROM cpp_job, job_to_finalized_at WHERE cpp_job.id = job_to_finalized_at.id AND cpp_job.state = 'running' FOR UPDATE), updated_job AS (UPDATE cpp_job SET finalized_at = job_to_update.finalized_at, state = 'completed' FROM job_to_update WHERE cpp_job.id = job_to_update.id RETURNING cpp_job.*) SELECT id, state, finalized_at FROM cpp_job WHERE id IN (SELECT id FROM job_to_finalized_at EXCEPT SELECT id FROM updated_job) UNION SELECT id, state, finalized_at FROM updated_job ORDER BY id; }
step s2done	{ WITH job_to_finalized_at AS (SELECT unnest(ARRAY[1, 2, 4]::bigint[]) AS id, unnest(ARRAY['2026-02-01', '2026-02-02', '2026-02-04']::timestamptz[]) AS finalized_at), job_to_update AS (SELECT cpp_job.id, job_to_finalized_at.finalized_at FROM cpp_job, job_to_finalized_at WHERE cpp_job.id = job_to_finalized_at.id AND cpp_job.state = 'running' FOR UPDATE), updated_job AS (UPDATE cpp_job SET finalized_at = job_to_update.finalized_at, state = 'completed' FROM job_to_update WHERE cpp_job.id = job_to_update.id RETURNING cpp_job.*) SELECT id, state, finalized_at FROM cpp_job WHERE id IN (SELECT id FROM job_to_finalized_at EXCEPT SELECT id FROM updated_job) UNION SELECT id, state, finalized_at FROM updated_job ORDER BY id; }
step s2show	{ SELECT id, state, finalized_at, metadata FROM cpp_job WHERE id IN (1, 2, 4) ORDER BY id; }

permutation s2x s1fin s2done s1c s2show
permutation s1meta s2done s1c s2show
permutation s1del s2done s1c s2show

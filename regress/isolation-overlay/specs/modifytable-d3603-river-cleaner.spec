# D3603: River's JobDeleteBefore (a data-modifying CTE's DELETE whose IN subquery holds a LIMIT, the subquery COPY-marked)
# against a concurrent change of a finished job (READ COMMITTED). The DELETE finds the version its snapshot read updated
# and rechecks the newest one under EvalPlanQual: the subquery's stored row is handed on and the subquery never runs
# again, so a job retried meanwhile (no longer finished) is still deleted (the IN test is on id only), and a job deleted
# meanwhile is skipped; the count is of the rows deleted. The expected output is PostgreSQL 18.6's.
setup
{
  CREATE TABLE rcl_job (id bigint PRIMARY KEY, state text NOT NULL, finalized_at timestamptz);
  INSERT INTO rcl_job SELECT g, CASE WHEN g % 2 = 0 THEN 'completed' ELSE 'available' END, CASE WHEN g % 2 = 0 THEN timestamptz '2026-01-01' + g * interval '1 minute' END FROM generate_series(1, 30) g;
  ANALYZE rcl_job;
}
teardown { DROP TABLE rcl_job; }

session s1
setup		{ BEGIN; }
step s1retry	{ UPDATE rcl_job SET state = 'available', finalized_at = NULL WHERE id = 2; }
step s1del	{ DELETE FROM rcl_job WHERE id = 4; }
step s1c	{ COMMIT; }

session s2
step s2x	{ EXPLAIN (COSTS OFF) WITH deleted_jobs AS (DELETE FROM rcl_job WHERE id IN (SELECT id FROM rcl_job WHERE state = 'completed' AND finalized_at < '2026-01-01 00:20' ORDER BY id LIMIT 4) RETURNING *) SELECT count(*) FROM deleted_jobs; }
step s2clean	{ WITH deleted_jobs AS (DELETE FROM rcl_job WHERE id IN (SELECT id FROM rcl_job WHERE state = 'completed' AND finalized_at < '2026-01-01 00:20' ORDER BY id LIMIT 4) RETURNING *) SELECT count(*), array_agg(id ORDER BY id) FROM deleted_jobs; }
step s2show	{ SELECT id, state FROM rcl_job WHERE id <= 10 ORDER BY id; }

permutation s2x s1retry s2clean s1c s2show
permutation s1del s2clean s1c s2show

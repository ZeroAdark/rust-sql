# D3750 / D3751 (realapp-graphile): graphile-worker 0.18's complete_job_q and fail_job_q, a data-modifying CTE's DELETE /
# UPDATE whose target meets a Function Scan by a nested loop, the function's row a COPY mark, the statement's UPDATE of the
# job's queue reading the CTE (READ COMMITTED). complete: `WITH j AS (DELETE FROM jobs USING unnest(ids) n(n) WHERE id = n
# RETURNING *) UPDATE queues .. FROM j ..`, PostgreSQL's Nested Loop whose outer is the Function Scan of unnest and whose
# inner is the jobs primary key's index scan by n. fail: `WITH j AS (UPDATE jobs SET .. FROM json_array_elements(list)
# els(el) WHERE id = (el->>'jobId')::bigint AND locked_by = pool RETURNING *) UPDATE queues .. FROM j ..`, a Nested Loop
# whose outer is the jobs' scan and whose inner is the Function Scan, its Join Filter the key. A worker's write waits on a
# job another session changed, then rechecks the newest version against the function's stored row: a changed payload
# passes (the queue is released), a job moved to another queue passes and releases that queue (RETURNING hands the newest
# version on), a deleted job and a renumbered one are skipped (the queue stays held), a job unlocked meanwhile fails the
# fail's locked_by test, and a fail over a job whose attempts changed computes run_at from the newest attempts. Each prints
# the jobs and the queues after. The expected output is PostgreSQL 18.6's.
setup
{
  CREATE TABLE d3750i_q (id int PRIMARY KEY, name text NOT NULL, locked_by text, locked_at timestamptz);
  CREATE TABLE d3750i_j (id bigint PRIMARY KEY, qid int, payload json NOT NULL DEFAULT json_build_object(), attempts smallint NOT NULL DEFAULT 0, run_at timestamptz NOT NULL, locked_by text, locked_at timestamptz, last_error text);
  INSERT INTO d3750i_q SELECT g, 'q' || g, CASE WHEN g <= 3 THEN 'pool-a' END, CASE WHEN g <= 3 THEN timestamptz '2026-10-01 11:00:00+00' END FROM generate_series(1, 4) g;
  INSERT INTO d3750i_j (id, qid, payload, attempts, run_at, locked_by, locked_at) SELECT g, CASE WHEN g <= 2 THEN g WHEN g <= 1000 THEN 4 END, json_build_object('n', g), 1, timestamptz '2026-10-01 00:00:00+00' + g * interval '1 second', CASE WHEN g <= 2 THEN 'pool-a' END, CASE WHEN g <= 2 THEN timestamptz '2026-10-01 11:00:00+00' END FROM generate_series(1, 2000) g;
  ANALYZE d3750i_q; ANALYZE d3750i_j;
}
teardown { DROP TABLE d3750i_j, d3750i_q; }

session s1
step s1upd	{ BEGIN; UPDATE d3750i_j SET payload = json_build_object('x', 1) WHERE id = 1; }
step s1mv	{ BEGIN; UPDATE d3750i_j SET qid = 3 WHERE id = 1; }
step s1del	{ BEGIN; DELETE FROM d3750i_j WHERE id = 1; }
step s1key	{ BEGIN; UPDATE d3750i_j SET id = 101 WHERE id = 1; }
step s1unl	{ BEGIN; UPDATE d3750i_j SET locked_by = NULL, locked_at = NULL WHERE id = 1; }
step s1att	{ BEGIN; UPDATE d3750i_j SET attempts = 9 WHERE id = 1; }
step s1c	{ COMMIT; }

session s2
step s2plc	{ EXPLAIN (COSTS OFF) WITH j AS (DELETE FROM d3750i_j AS jobs USING unnest(ARRAY[1, 2]::bigint[]) n(n) WHERE id = n RETURNING *) UPDATE d3750i_q AS job_queues SET locked_by = null, locked_at = null FROM j WHERE job_queues.id = j.qid AND job_queues.locked_by = 'pool-a'; }
step s2plf	{ EXPLAIN (COSTS OFF) WITH j AS (UPDATE d3750i_j AS jobs SET last_error = (el->>'message'), run_at = greatest(timestamptz '2026-10-01 12:00:00+00', run_at) + (exp(least(attempts, 10)) * interval '1 second'), locked_by = null, locked_at = null, payload = coalesce(el->'payload', jobs.payload) FROM json_array_elements(json_build_array(json_build_object('jobId', '1', 'message', 'm1'), json_build_object('jobId', '2', 'message', 'm2', 'payload', json_build_array(2)))) AS els(el) WHERE id = (el->>'jobId')::bigint AND locked_by = 'pool-a' RETURNING *) UPDATE d3750i_q AS job_queues SET locked_by = null, locked_at = null FROM j WHERE job_queues.id = j.qid AND job_queues.locked_by = 'pool-a'; }
step s2comp	{ WITH j AS (DELETE FROM d3750i_j AS jobs USING unnest(ARRAY[1, 2]::bigint[]) n(n) WHERE id = n RETURNING *) UPDATE d3750i_q AS job_queues SET locked_by = null, locked_at = null FROM j WHERE job_queues.id = j.qid AND job_queues.locked_by = 'pool-a'; }
step s2fail	{ WITH j AS (UPDATE d3750i_j AS jobs SET last_error = (el->>'message'), run_at = greatest(timestamptz '2026-10-01 12:00:00+00', run_at) + (exp(least(attempts, 10)) * interval '1 second'), locked_by = null, locked_at = null, payload = coalesce(el->'payload', jobs.payload) FROM json_array_elements(json_build_array(json_build_object('jobId', '1', 'message', 'm1'), json_build_object('jobId', '2', 'message', 'm2', 'payload', json_build_array(2)))) AS els(el) WHERE id = (el->>'jobId')::bigint AND locked_by = 'pool-a' RETURNING *) UPDATE d3750i_q AS job_queues SET locked_by = null, locked_at = null FROM j WHERE job_queues.id = j.qid AND job_queues.locked_by = 'pool-a'; }

session s3
step s3show	{ SELECT id, qid, payload, attempts, run_at, locked_by, last_error FROM d3750i_j WHERE id IN (1, 2, 101) ORDER BY id; SELECT id, locked_by, locked_at FROM d3750i_q ORDER BY id; }

permutation s2plc s2plf
permutation s1upd s2comp s1c s3show
permutation s1mv s2comp s1c s3show
permutation s1del s2comp s1c s3show
permutation s1key s2comp s1c s3show
permutation s1upd s2fail s1c s3show
permutation s1mv s2fail s1c s3show
permutation s1unl s2fail s1c s3show
permutation s1att s2fail s1c s3show

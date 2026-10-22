# D3601 (2): River's completer's updated_metadata_only, a data-modifying CTE's UPDATE whose target scan runs a hashed
# NOT IN SubPlan and probes the hash of a CTE of unnest columns, against a concurrent change of a listed row (READ
# COMMITTED). The write finds the version its snapshot read updated and rechecks the newest one under EvalPlanQual,
# whose fresh SubPlan state builds the set again under the statement's snapshot: a row whose metadata changed is written
# over its newest version, one set running is skipped, one deleted is gone, and one whose id the concurrent transaction
# also added to the NOT IN table is still written (the set is the snapshot's); the same over a NOT IN of another
# data-modifying CTE's store. The expected output is PostgreSQL 18.6's.
setup
{
  CREATE TABLE rj (id bigint PRIMARY KEY, state text NOT NULL, meta jsonb NOT NULL DEFAULT jsonb_build_object());
  INSERT INTO rj SELECT g, CASE WHEN g % 3 = 0 THEN 'running' ELSE 'available' END, jsonb_build_object() FROM generate_series(1, 60) g;
  CREATE TABLE rj_done (id bigint PRIMARY KEY);
  INSERT INTO rj_done VALUES (5), (50);
  ANALYZE rj; ANALYZE rj_done;
}
teardown { DROP TABLE rj, rj_done; }

session s1
setup		{ BEGIN; }
step s1meta	{ UPDATE rj SET meta = meta || jsonb_build_object('s1', true) WHERE id = 4; }
step s1run	{ UPDATE rj SET state = 'running' WHERE id = 4; }
step s1del	{ DELETE FROM rj WHERE id = 4; }
step s1done	{ UPDATE rj SET meta = meta || jsonb_build_object('s1', true) WHERE id = 4; INSERT INTO rj_done VALUES (4); }
step s1c	{ COMMIT; }

session s2
step s2x	{ EXPLAIN (COSTS OFF) WITH job_input AS (SELECT unnest(ARRAY[4, 5, 6, 7, 8]::bigint[]) AS id, unnest(ARRAY[true, true, true, true, false]) AS mm), um AS (UPDATE rj SET meta = rj.meta || jsonb_build_object('k', 1) FROM job_input WHERE rj.id = job_input.id AND rj.id NOT IN (SELECT id FROM rj_done) AND rj.state != 'running' AND job_input.mm RETURNING rj.id, rj.meta) SELECT id, meta FROM um ORDER BY id; }
step s2cte	{ WITH job_input AS (SELECT unnest(ARRAY[4, 5, 6, 7, 8]::bigint[]) AS id, unnest(ARRAY[true, true, true, true, false]) AS mm), um AS (UPDATE rj SET meta = rj.meta || jsonb_build_object('k', 1) FROM job_input WHERE rj.id = job_input.id AND rj.id NOT IN (SELECT id FROM rj_done) AND rj.state != 'running' AND job_input.mm RETURNING rj.id, rj.meta) SELECT id, meta FROM um ORDER BY id; }
step s2ur	{ WITH job_input AS (SELECT unnest(ARRAY[4, 5, 6, 7, 8]::bigint[]) AS id, unnest(ARRAY[true, true, true, true, false]) AS mm), ur AS (UPDATE rj SET state = 'completed' WHERE id IN (7, 9) RETURNING id), um AS (UPDATE rj SET meta = rj.meta || jsonb_build_object('k', 2) FROM job_input WHERE rj.id = job_input.id AND rj.id NOT IN (SELECT id FROM ur) AND rj.state != 'running' AND job_input.mm RETURNING rj.id, rj.meta) SELECT id, meta FROM um ORDER BY id; }
step s2show	{ SELECT id, state, meta FROM rj WHERE id BETWEEN 4 AND 8 ORDER BY id; }

permutation s2x s1meta s2cte s1c s2show
permutation s1run s2cte s1c s2show
permutation s1del s2cte s1c s2show
permutation s1done s2cte s1c s2show
permutation s1meta s2ur s1c s2show
permutation s1run s2ur s1c s2show

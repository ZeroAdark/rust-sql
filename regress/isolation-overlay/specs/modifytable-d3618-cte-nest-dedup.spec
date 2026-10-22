# D3618: pg-boss's single-job complete (a data-modifying CTE's UPDATE whose target is the inner of a Nested Loop over the
# HashAggregate of the IN subquery's unnest rows) against a concurrent change of the job (READ COMMITTED). The write
# waits, then rechecks the newest version joined to the id the nest holds: a job another worker completed meanwhile is
# skipped, an active job whose data changed meanwhile is completed, a deleted one is skipped. The expected output is
# PostgreSQL 18.6's.
setup
{
  CREATE TABLE ndc (id int NOT NULL, name text NOT NULL, state text NOT NULL, data int NOT NULL DEFAULT 0, completed_on timestamptz, PRIMARY KEY (name, id));
  INSERT INTO ndc SELECT g, CASE WHEN g % 2 = 0 THEN 'q1' ELSE 'q2' END, 'active' FROM generate_series(1, 600) g;
  ANALYZE ndc;
}
teardown { DROP TABLE ndc; }

session s1
setup		{ BEGIN; }
step s1done	{ UPDATE ndc SET state = 'completed' WHERE name = 'q1' AND id = 2; }
step s1data	{ UPDATE ndc SET data = data + 1 WHERE name = 'q1' AND id = 2; }
step s1del	{ DELETE FROM ndc WHERE name = 'q1' AND id = 2; }
step s1c	{ COMMIT; }

session s2
step s2x	{ EXPLAIN (COSTS OFF) WITH results AS (UPDATE ndc SET completed_on = '2026-02-01', state = 'completed' WHERE name = 'q1' AND id IN (SELECT UNNEST(ARRAY[2])) AND state = 'active' RETURNING *) SELECT COUNT(*) FROM results; }
step s2done	{ WITH results AS (UPDATE ndc SET completed_on = '2026-02-01', state = 'completed' WHERE name = 'q1' AND id IN (SELECT UNNEST(ARRAY[2])) AND state = 'active' RETURNING *) SELECT COUNT(*) FROM results; }
step s2show	{ SELECT name, id, state, data, completed_on FROM ndc WHERE id = 2; }

permutation s2x s1done s2done s1c s2show
permutation s1data s2done s1c s2show
permutation s1del s2done s1c s2show

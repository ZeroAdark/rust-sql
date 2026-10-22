# D3617: pg-boss 10's completeJobs over its LIST-partitioned job table (the root pruned to the queue's partition, the
# write in a data-modifying CTE) against a concurrent change of a job (READ COMMITTED). The write waits, then rechecks the
# newest version: a job another worker completed meanwhile (no longer active) is skipped, an active job whose data
# changed meanwhile is completed, a deleted one is skipped, and one a concurrent update moved to another queue's
# partition raises, as PostgreSQL's lock of a moved row does. The expected output is PostgreSQL 18.6's.
setup
{
  CREATE TABLE pbc (id int NOT NULL, name text NOT NULL, state text NOT NULL, data int NOT NULL DEFAULT 0, completed_on timestamptz, PRIMARY KEY (name, id)) PARTITION BY LIST (name);
  CREATE TABLE pbc_q1 PARTITION OF pbc FOR VALUES IN ('q1');
  CREATE TABLE pbc_q2 PARTITION OF pbc FOR VALUES IN ('q2');
  INSERT INTO pbc SELECT g, CASE WHEN g % 2 = 0 THEN 'q1' ELSE 'q2' END, 'active' FROM generate_series(1, 400) g;
  ANALYZE pbc;
}
teardown { DROP TABLE pbc; }

session s1
setup		{ BEGIN; }
step s1done	{ UPDATE pbc SET state = 'completed' WHERE name = 'q1' AND id = 2; }
step s1data	{ UPDATE pbc SET data = data + 1 WHERE name = 'q1' AND id = 2; }
step s1del	{ DELETE FROM pbc WHERE name = 'q1' AND id = 4; }
step s1move	{ UPDATE pbc SET name = 'q2' WHERE name = 'q1' AND id = 2; }
step s1c	{ COMMIT; }

session s2
step s2x	{ EXPLAIN (COSTS OFF) WITH results AS (UPDATE pbc SET completed_on = '2026-02-01', state = 'completed' WHERE name = 'q1' AND id IN (SELECT UNNEST(ARRAY[2, 4, 6])) AND state = 'active' RETURNING *) SELECT COUNT(*) FROM results; }
step s2done	{ WITH results AS (UPDATE pbc SET completed_on = '2026-02-01', state = 'completed' WHERE name = 'q1' AND id IN (SELECT UNNEST(ARRAY[2, 4, 6])) AND state = 'active' RETURNING *) SELECT COUNT(*) FROM results; }
step s2show	{ SELECT name, id, state, data, completed_on FROM pbc WHERE id IN (2, 4, 6) ORDER BY id; }

permutation s2x s1done s2done s1c s2show
permutation s1data s2done s1c s2show
permutation s1del s2done s1c s2show
permutation s1move s2done s1c s2show

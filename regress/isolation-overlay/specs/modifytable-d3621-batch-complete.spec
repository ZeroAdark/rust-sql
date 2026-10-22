# D3621: pg-boss's batch complete (an UPDATE by a two-id list, at the top level and in a data-modifying CTE, over a Nested
# Loop whose outer is the Unique over the Sort of the IN subquery's unnest rows, its inner the job table's index scan)
# against a concurrent change of one of the jobs (READ COMMITTED). The write waits, then rechecks the newest version
# joined to the id the nest holds: a job another worker completed meanwhile is skipped, an active job whose data changed
# meanwhile is completed, a deleted one is skipped; the other job is completed either way. The expected output is
# PostgreSQL 18.6's.
setup
{
  CREATE TABLE usc (id int NOT NULL, name text NOT NULL, state text NOT NULL, data int NOT NULL DEFAULT 0, completed_on timestamptz, PRIMARY KEY (name, id));
  INSERT INTO usc SELECT g, CASE WHEN g % 2 = 0 THEN 'q1' ELSE 'q2' END, 'active' FROM generate_series(1, 20000) g;
  ANALYZE usc;
}
teardown { DROP TABLE usc; }

session s1
setup		{ BEGIN; }
step s1done	{ UPDATE usc SET state = 'completed' WHERE name = 'q1' AND id = 2; }
step s1data	{ UPDATE usc SET data = data + 1 WHERE name = 'q1' AND id = 2; }
step s1del	{ DELETE FROM usc WHERE name = 'q1' AND id = 2; }
step s1c	{ COMMIT; }

session s2
step s2x	{ EXPLAIN (COSTS OFF) UPDATE usc SET completed_on = '2026-02-01', state = 'completed' WHERE name = 'q1' AND id IN (SELECT UNNEST(ARRAY[2, 4])) AND state = 'active'; }
step s2top	{ UPDATE usc SET completed_on = '2026-02-01', state = 'completed' WHERE name = 'q1' AND id IN (SELECT UNNEST(ARRAY[2, 4])) AND state = 'active' RETURNING id, state, data; }
step s2cte	{ WITH results AS (UPDATE usc SET completed_on = '2026-02-01', state = 'completed' WHERE name = 'q1' AND id IN (SELECT UNNEST(ARRAY[2, 4])) AND state = 'active' RETURNING *) SELECT COUNT(*) FROM results; }
step s2show	{ SELECT name, id, state, data, completed_on FROM usc WHERE id IN (2, 4) ORDER BY id; }

permutation s2x s1done s2top s1c s2show
permutation s1data s2top s1c s2show
permutation s1del s2top s1c s2show
permutation s1done s2cte s1c s2show
permutation s1data s2cte s1c s2show
permutation s1del s2cte s1c s2show

# D3567 REFCHAIN: Airflow's claim of executable task instances, Limit -> LockRows -> Sort over two joins (dag_run and dag
# REFERENCE-marked). Over a newer version of a task instance (READ COMMITTED) PostgreSQL's EvalPlanQual refetches each
# REFERENCE row at its junk ctid (the version the join read) and reruns the joins over the newest task instance: one
# still scheduled is claimed with its new priority where the Sort put it; one queued meanwhile is skipped and the Limit
# takes the next; one moved to another run fails the refetched run's join (skipped); a dag paused in the same
# transaction still passes (its refetched row is the version the join read). SKIP LOCKED. s2: the hash join then the nest
# over the dag pkey; s3: two hash joins; s4: two nests. Expected output is PostgreSQL 18.6's.
setup
{
  CREATE TABLE cdag (dag_id text PRIMARY KEY, is_paused bool NOT NULL DEFAULT false);
  CREATE TABLE crun (id serial PRIMARY KEY, dag_id text NOT NULL, run_id text NOT NULL, state text, run_type text NOT NULL, execution_date timestamptz NOT NULL, UNIQUE (dag_id, run_id));
  CREATE TABLE cti (dag_id text NOT NULL, task_id text NOT NULL, run_id text NOT NULL, map_index int NOT NULL DEFAULT -1, state text, priority_weight int NOT NULL DEFAULT 1, PRIMARY KEY (dag_id, task_id, run_id, map_index));
  INSERT INTO cdag SELECT 'd' || g, g % 7 = 0 FROM generate_series(1, 40) g;
  INSERT INTO crun (dag_id, run_id, state, run_type, execution_date) SELECT 'd' || (g % 40 + 1), 'r' || g, CASE WHEN g % 3 = 0 THEN 'success' ELSE 'running' END, CASE WHEN g % 10 = 0 THEN 'backfill' ELSE 'scheduled' END, timestamptz '2026-01-01' + g * interval '1 minute' FROM generate_series(1, 200) g;
  INSERT INTO cti (dag_id, task_id, run_id, state, priority_weight) SELECT 'd' || ((g % 200 + 1) % 40 + 1), 't' || (g / 200), 'r' || (g % 200 + 1), (ARRAY['scheduled', 'queued', 'running', 'success', 'scheduled'])[g % 5 + 1], g % 11 FROM generate_series(1, 1000) g;
  ANALYZE cdag, crun, cti;
}
teardown { DROP TABLE cti, crun, cdag; }

session s1
setup		{ BEGIN; }
step s1a	{ UPDATE cti SET priority_weight = 0 WHERE (dag_id, task_id, run_id) = ('d12', 't0', 'r11'); }
step s1q	{ UPDATE cti SET state = 'queued' WHERE (dag_id, task_id, run_id) = ('d12', 't0', 'r11'); }
step s1m	{ UPDATE cti SET run_id = 'r12' WHERE (dag_id, task_id, run_id) = ('d12', 't0', 'r11'); }
step s1p	{ UPDATE cdag SET is_paused = true WHERE dag_id = 'd12'; UPDATE cti SET priority_weight = 10 WHERE (dag_id, task_id, run_id) = ('d12', 't0', 'r11'); }
step s1k	{ SELECT 1 FROM cti WHERE (dag_id, task_id, run_id) = ('d12', 't0', 'r11') FOR UPDATE; }
step s1c	{ COMMIT; }
teardown	{ ROLLBACK; }

session s2
step s2x	{ EXPLAIN (COSTS OFF) SELECT cti.dag_id, cti.task_id, cti.run_id, cti.priority_weight FROM cti JOIN crun ON crun.dag_id = cti.dag_id AND crun.run_id = cti.run_id JOIN cdag ON cti.dag_id = cdag.dag_id WHERE crun.state = 'running' AND crun.run_type <> 'backfill' AND cti.state = 'scheduled' AND cdag.is_paused = false ORDER BY -cti.priority_weight, crun.execution_date, cti.map_index, cti.task_id LIMIT 4 FOR UPDATE OF cti; }
step s2l	{ SELECT cti.dag_id, cti.task_id, cti.run_id, cti.priority_weight FROM cti JOIN crun ON crun.dag_id = cti.dag_id AND crun.run_id = cti.run_id JOIN cdag ON cti.dag_id = cdag.dag_id WHERE crun.state = 'running' AND crun.run_type <> 'backfill' AND cti.state = 'scheduled' AND cdag.is_paused = false ORDER BY -cti.priority_weight, crun.execution_date, cti.map_index, cti.task_id LIMIT 4 FOR UPDATE OF cti; }
step s2sk	{ SELECT cti.dag_id, cti.task_id, cti.run_id FROM cti JOIN crun ON crun.dag_id = cti.dag_id AND crun.run_id = cti.run_id JOIN cdag ON cti.dag_id = cdag.dag_id WHERE crun.state = 'running' AND crun.run_type <> 'backfill' AND cti.state = 'scheduled' AND cdag.is_paused = false ORDER BY -cti.priority_weight, crun.execution_date, cti.map_index, cti.task_id LIMIT 3 FOR UPDATE OF cti SKIP LOCKED; }

session s3
setup		{ SET enable_nestloop = off; SET enable_mergejoin = off; }
step s3x	{ EXPLAIN (COSTS OFF) SELECT cti.dag_id, cti.task_id, cti.run_id, cti.priority_weight FROM cti JOIN crun ON crun.dag_id = cti.dag_id AND crun.run_id = cti.run_id JOIN cdag ON cti.dag_id = cdag.dag_id WHERE crun.state = 'running' AND crun.run_type <> 'backfill' AND cti.state = 'scheduled' AND cdag.is_paused = false ORDER BY -cti.priority_weight, crun.execution_date, cti.map_index, cti.task_id LIMIT 4 FOR UPDATE OF cti; }
step s3l	{ SELECT cti.dag_id, cti.task_id, cti.run_id, cti.priority_weight FROM cti JOIN crun ON crun.dag_id = cti.dag_id AND crun.run_id = cti.run_id JOIN cdag ON cti.dag_id = cdag.dag_id WHERE crun.state = 'running' AND crun.run_type <> 'backfill' AND cti.state = 'scheduled' AND cdag.is_paused = false ORDER BY -cti.priority_weight, crun.execution_date, cti.map_index, cti.task_id LIMIT 4 FOR UPDATE OF cti; }

session s4
setup		{ SET enable_hashjoin = off; SET enable_mergejoin = off; }
step s4x	{ EXPLAIN (COSTS OFF) SELECT cti.dag_id, cti.task_id, cti.run_id, cti.priority_weight FROM cti JOIN crun ON crun.dag_id = cti.dag_id AND crun.run_id = cti.run_id JOIN cdag ON cti.dag_id = cdag.dag_id WHERE crun.state = 'running' AND crun.run_type <> 'backfill' AND cti.state = 'scheduled' AND cdag.is_paused = false ORDER BY -cti.priority_weight, crun.execution_date, cti.map_index, cti.task_id LIMIT 4 FOR UPDATE OF cti; }
step s4l	{ SELECT cti.dag_id, cti.task_id, cti.run_id, cti.priority_weight FROM cti JOIN crun ON crun.dag_id = cti.dag_id AND crun.run_id = cti.run_id JOIN cdag ON cti.dag_id = cdag.dag_id WHERE crun.state = 'running' AND crun.run_type <> 'backfill' AND cti.state = 'scheduled' AND cdag.is_paused = false ORDER BY -cti.priority_weight, crun.execution_date, cti.map_index, cti.task_id LIMIT 4 FOR UPDATE OF cti; }

# the claim order: priority 10 first (-10), then the run's execution_date: ('d2', 't5', 'r1'), ('d6', 't3', 'r5'), ('d12', 't0', 'r11'), ('d17', 't3', 'r16'), ('d26', 't4', 'r25'), ..
permutation s2x s1a s2l s1c
permutation s1q s2l s1c
permutation s1m s2l s1c
permutation s1p s2l s1c
permutation s1k s2sk s1c
permutation s3x s1a s3l s1c
permutation s1m s3l s1c
permutation s1p s3l s1c
permutation s4x s1a s4l s1c
permutation s1m s4l s1c
permutation s1p s4l s1c

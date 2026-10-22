# lockrows REFBUILD sorted (gen queue-0015 under the e2e rig's plan): Limit -> LockRows -> Sort -> Hash Join, the locked
# relation j (FOR UPDATE OF j) built from its own filtered scan, the REFERENCE-marked w probing it. Over a newer version of
# j (READ COMMITTED) PostgreSQL's EvalPlanQual reruns the Sort over the join of the test tuple and w refetched at the
# row's junk ctid (the version the join read): j's newest version through its filter and the hash clause, then the
# projection; LockRows hands the row to the Limit, which counts it. A newer j whose state no longer passes, or whose
# worker_id names another worker, is skipped and the Limit takes the next row; a renamed w keeps the name the join read.
# SKIP LOCKED skips a row s1 holds; NOWAIT raises 55P03. Expected output is PostgreSQL 18.6's.
setup
{
  CREATE TABLE rsw (id int PRIMARY KEY, name text NOT NULL);
  INSERT INTO rsw SELECT i, 'w' || i FROM generate_series(1, 2000) i;
  CREATE TABLE rsj (id int PRIMARY KEY, state text NOT NULL, worker_id int, attempts int NOT NULL DEFAULT 0);
  INSERT INTO rsj SELECT i, CASE WHEN i % 100 = 8 THEN 'running' ELSE 'done' END, 1 + i % 2000, 0 FROM generate_series(1, 4000) i;
  CREATE INDEX rsj_state ON rsj (state);
  ANALYZE rsw, rsj;
}
teardown { DROP TABLE rsj, rsw; }

session s1
setup		{ BEGIN; }
step s1a	{ UPDATE rsj SET attempts = attempts + 1 WHERE id = 108; }
step s1s	{ UPDATE rsj SET state = 'done' WHERE id = 108; }
step s1w	{ UPDATE rsj SET worker_id = 3 WHERE id = 108; }
step s1n	{ UPDATE rsw SET name = 'renamed' WHERE id = 109; }
step s1l	{ SELECT id FROM rsj WHERE id = 108 FOR UPDATE; }
step s1d	{ DELETE FROM rsj WHERE id = 8; }
step s1c	{ COMMIT; }

session s2
setup		{ SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_indexscan = off; }
step s2x	{ EXPLAIN (COSTS OFF) SELECT j.id, w.name FROM rsj j JOIN rsw w ON w.id = j.worker_id WHERE j.state = 'running' ORDER BY j.id LIMIT 3 FOR UPDATE OF j; }
step s2l	{ SELECT j.id, j.attempts, w.name FROM rsj j JOIN rsw w ON w.id = j.worker_id WHERE j.state = 'running' ORDER BY j.id LIMIT 3 FOR UPDATE OF j; }
step s2o	{ SELECT j.id, w.name FROM rsj j JOIN rsw w ON w.id = j.worker_id WHERE j.state = 'running' ORDER BY j.id LIMIT 2 OFFSET 1 FOR UPDATE OF j; }
step s2sk	{ SELECT j.id, w.name FROM rsj j JOIN rsw w ON w.id = j.worker_id WHERE j.state = 'running' ORDER BY j.id LIMIT 3 FOR UPDATE OF j SKIP LOCKED; }
step s2nw	{ SELECT j.id, w.name FROM rsj j JOIN rsw w ON w.id = j.worker_id WHERE j.state = 'running' ORDER BY j.id LIMIT 3 FOR UPDATE OF j NOWAIT; }

permutation s2x s1a s2l s1c
permutation s1s s2l s1c
permutation s1w s2l s1c
permutation s1n s1a s2l s1c
permutation s1d s2l s1c
permutation s1s s2o s1c
permutation s1l s2sk s1c
permutation s1l s2nw s1c

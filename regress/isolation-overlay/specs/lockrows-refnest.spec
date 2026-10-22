# lockrows REFNEST (gen queue-0015): Limit -> LockRows -> Nested Loop, the outer the locked relation's own index scan (FOR
# UPDATE OF j), the inner the REFERENCE-marked relation's parameterized pkey probe, a Memoize over it once the tables are
# analyzed (s2; s3 with enable_memoize off: the probe alone). Over a newer version of j (READ COMMITTED) PostgreSQL's
# EvalPlanQual reruns the join over the test tuple: j's newest version through its index quals and filter, the nest's
# parameter from it, w refetched at the row's junk ctid (the version the join read) and rechecked under that parameter
# (IndexRecheck: w.id = j.worker_id), then the projection; LockRows hands the row to the Limit, which counts it. A newer j
# whose state no longer passes, or whose worker_id names another worker (the refetched w fails the recheck), is skipped
# and the Limit takes the next row; a renamed w keeps the name the join read. SKIP LOCKED skips a row s1 holds; NOWAIT
# raises 55P03. s4 (gen jcore-0294, analyzed): LockRows -> Nested Loop, the inner a Materialize over the REFERENCE
# relation's unparameterized pkey probe: the recheck redoes that probe over the refetched row (a fresh Materialize in the
# EPQ tree). Expected output is PostgreSQL 18.6's.
setup
{
  CREATE TABLE rnw (id int PRIMARY KEY, name text NOT NULL);
  INSERT INTO rnw SELECT i, 'w' || i FROM generate_series(1, 20) i;
  CREATE TABLE rnj (id int PRIMARY KEY, state text NOT NULL, worker_id int, attempts int NOT NULL DEFAULT 0);
  INSERT INTO rnj SELECT i, CASE WHEN i % 10 = 8 THEN 'running' ELSE 'done' END, CASE WHEN i % 10 = 8 THEN 1 + i % 20 END, 0 FROM generate_series(1, 2000) i;
  CREATE TABLE rnp (id int PRIMARY KEY, name text NOT NULL);
  INSERT INTO rnp SELECT i, 'p' || i FROM generate_series(1, 2000) i;
  CREATE TABLE rni (wh int NOT NULL, pid int NOT NULL, qty int NOT NULL, PRIMARY KEY (wh, pid));
  INSERT INTO rni SELECT w, p, (w * 31 + p * 17) % 100 FROM generate_series(1, 8) w, generate_series(1, 2000) p WHERE (w + p) % 13 <> 0;
  ANALYZE rnw, rnj, rnp, rni;
}
teardown { DROP TABLE rnj, rnw, rni, rnp; }

session s1
setup		{ BEGIN; }
step s1a	{ UPDATE rnj SET attempts = attempts + 1 WHERE id = 8; }
step s1s	{ UPDATE rnj SET state = 'done' WHERE id = 8; }
step s1w	{ UPDATE rnj SET worker_id = 3 WHERE id = 8; }
step s1n	{ UPDATE rnw SET name = 'renamed' WHERE id = 9; }
step s1l	{ SELECT id FROM rnj WHERE id = 8 FOR UPDATE; }
step s1d	{ DELETE FROM rnj WHERE id = 18; }
step s1q	{ UPDATE rni SET qty = qty + 100 WHERE wh = 2 AND pid = 42; }
step s1m	{ UPDATE rni SET pid = 43 WHERE wh = 3 AND pid = 42; }
step s1c	{ COMMIT; }
teardown	{ ROLLBACK; }

session s2
setup		{ SET enable_seqscan = off; SET enable_bitmapscan = off; SET enable_hashjoin = off; SET enable_mergejoin = off; SET enable_sort = off; }
step s2x	{ EXPLAIN (COSTS OFF) SELECT j.id, w.name FROM rnj j JOIN rnw w ON w.id = j.worker_id WHERE j.state = 'running' ORDER BY j.id LIMIT 3 FOR UPDATE OF j; }
step s2l	{ SELECT j.id, w.name FROM rnj j JOIN rnw w ON w.id = j.worker_id WHERE j.state = 'running' ORDER BY j.id LIMIT 3 FOR UPDATE OF j; }
step s2o	{ SELECT j.id, w.name FROM rnj j JOIN rnw w ON w.id = j.worker_id WHERE j.state = 'running' ORDER BY j.id LIMIT 2 OFFSET 1 FOR UPDATE OF j; }
step s2sk	{ SELECT j.id, w.name FROM rnj j JOIN rnw w ON w.id = j.worker_id WHERE j.state = 'running' ORDER BY j.id LIMIT 3 FOR UPDATE OF j SKIP LOCKED; }
step s2nw	{ SELECT j.id, w.name FROM rnj j JOIN rnw w ON w.id = j.worker_id WHERE j.state = 'running' ORDER BY j.id LIMIT 3 FOR UPDATE OF j NOWAIT; }

session s3
setup		{ SET enable_seqscan = off; SET enable_bitmapscan = off; SET enable_hashjoin = off; SET enable_mergejoin = off; SET enable_sort = off; SET enable_memoize = off; }
step s3x	{ EXPLAIN (COSTS OFF) SELECT j.id, w.name FROM rnj j JOIN rnw w ON w.id = j.worker_id WHERE j.state = 'running' ORDER BY j.id LIMIT 3 FOR UPDATE OF j; }
step s3l	{ SELECT j.id, w.name FROM rnj j JOIN rnw w ON w.id = j.worker_id WHERE j.state = 'running' ORDER BY j.id LIMIT 3 FOR UPDATE OF j; }

session s4
setup		{ SET enable_seqscan = off; SET enable_bitmapscan = off; SET enable_hashjoin = off; SET enable_mergejoin = off; }
step s4x	{ EXPLAIN (COSTS OFF) SELECT i.wh, i.qty FROM rni i JOIN rnp p ON p.id = i.pid WHERE p.id = 42 ORDER BY i.wh FOR UPDATE OF i; }
step s4l	{ SELECT i.wh, i.qty, p.name FROM rni i JOIN rnp p ON p.id = i.pid WHERE p.id = 42 ORDER BY i.wh FOR UPDATE OF i; }

# running: ids 8, 18, 28, ..; worker_id = 1 + id % 20: 9, 19, 9, ..
permutation s2x s1a s2l s1c
permutation s1s s2l s1c
permutation s1w s2l s1c
permutation s1n s1a s2l s1c
permutation s1d s2l s1c
permutation s1s s2o s1c
permutation s1l s2sk s1c
permutation s1l s2nw s1c
permutation s3x s1a s3l s1c
permutation s1s s3l s1c
permutation s1w s3l s1c
permutation s4x s1q s4l s1c
permutation s1m s4l s1c

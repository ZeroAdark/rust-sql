# D3554 SORTNEST: Django's claim over a join sorted past its index, Limit -> LockRows -> Incremental Sort -> Nested Loop
# (the outer the locked relation's own index scan, FOR UPDATE OF t; the inner the REFERENCE-marked queue's scan). Over a newer version of t (READ COMMITTED) PostgreSQL's EvalPlanQual reruns the Sort's subtree over the
# test tuple: t's newest version through its index quals and filters, the nest's parameter from it, q refetched at the
# row's junk ctid (the version the join read) and rechecked under that parameter, then the projection; LockRows hands the
# row to the Limit, which counts it. A newer t that is no longer ready, or whose queue_id names another queue (the
# refetched q fails the recheck), is skipped and the Limit takes the next row; a paused queue keeps the version the join
# read. SKIP LOCKED skips a row s1 holds; NOWAIT raises 55P03. s2's inner is a Materialize over the queue's seq scan (the
# join filter q.id = t.queue_id); s3 a Memoize over the queue's pkey probe (enable_material off); s5 the probe alone
# (enable_memoize off too); s4 a Sort the index does not give over the Materialize form. Expected output is PostgreSQL 18.6's.
setup
{
  CREATE TABLE snq (id int PRIMARY KEY, name text NOT NULL, paused bool NOT NULL DEFAULT false, max_attempts int NOT NULL DEFAULT 5);
  CREATE TABLE snt (id bigint PRIMARY KEY, queue_id int NOT NULL, status text NOT NULL, priority int NOT NULL, run_at timestamptz NOT NULL, attempts int NOT NULL DEFAULT 0);
  CREATE INDEX snt_claim ON snt (status, priority DESC, run_at) WHERE status = 'ready';
  INSERT INTO snq SELECT g, 'q' || g, g = 3 FROM generate_series(1, 5) g;
  INSERT INTO snt SELECT g, g % 5 + 1, CASE WHEN g % 6 = 0 THEN 'done' ELSE 'ready' END, g % 4, timestamptz '2026-01-01' + g * interval '1 minute', g % 3 FROM generate_series(1, 400) g;
  ANALYZE snq, snt;
}
teardown { DROP TABLE snt, snq; }

session s1
setup		{ BEGIN; }
step s1a	{ UPDATE snt SET attempts = attempts + 1 WHERE id = 3; }
step s1s	{ UPDATE snt SET status = 'done' WHERE id = 3; }
step s1p	{ UPDATE snt SET priority = 0, run_at = run_at + interval '1 day' WHERE id = 3; }
step s1q	{ UPDATE snt SET queue_id = 3 WHERE id = 3; }
step s1n	{ UPDATE snq SET paused = true, name = 'renamed' WHERE id = 4; }
step s1d	{ DELETE FROM snt WHERE id = 11; }
step s1l	{ SELECT id FROM snt WHERE id = 3 FOR UPDATE; }
step s1c	{ COMMIT; }
teardown	{ ROLLBACK; }

session s2
step s2x	{ EXPLAIN (COSTS OFF) SELECT t.id, q.name FROM snt t JOIN snq q ON q.id = t.queue_id WHERE t.status = 'ready' AND NOT q.paused ORDER BY t.priority DESC, t.run_at, t.id LIMIT 3 FOR UPDATE OF t; }
step s2l	{ SELECT t.id, t.priority, t.attempts, q.name FROM snt t JOIN snq q ON q.id = t.queue_id WHERE t.status = 'ready' AND NOT q.paused ORDER BY t.priority DESC, t.run_at, t.id LIMIT 3 FOR UPDATE OF t; }
step s2o	{ SELECT t.id, q.name FROM snt t JOIN snq q ON q.id = t.queue_id WHERE t.status = 'ready' AND NOT q.paused ORDER BY t.priority DESC, t.run_at, t.id LIMIT 2 OFFSET 1 FOR UPDATE OF t; }
step s2sk	{ SELECT t.id, q.name FROM snt t JOIN snq q ON q.id = t.queue_id WHERE t.status = 'ready' AND NOT q.paused ORDER BY t.priority DESC, t.run_at, t.id LIMIT 3 FOR UPDATE OF t SKIP LOCKED; }
step s2nw	{ SELECT t.id, q.name FROM snt t JOIN snq q ON q.id = t.queue_id WHERE t.status = 'ready' AND NOT q.paused ORDER BY t.priority DESC, t.run_at, t.id LIMIT 3 FOR UPDATE OF t NOWAIT; }

session s3
setup		{ SET enable_material = off; }
step s3x	{ EXPLAIN (COSTS OFF) SELECT t.id, q.name FROM snt t JOIN snq q ON q.id = t.queue_id WHERE t.status = 'ready' AND NOT q.paused ORDER BY t.priority DESC, t.run_at, t.id LIMIT 3 FOR UPDATE OF t; }
step s3l	{ SELECT t.id, t.attempts, q.name FROM snt t JOIN snq q ON q.id = t.queue_id WHERE t.status = 'ready' AND NOT q.paused ORDER BY t.priority DESC, t.run_at, t.id LIMIT 3 FOR UPDATE OF t; }

session s5
setup		{ SET enable_material = off; SET enable_memoize = off; }
step s5x	{ EXPLAIN (COSTS OFF) SELECT t.id, q.name FROM snt t JOIN snq q ON q.id = t.queue_id WHERE t.status = 'ready' AND NOT q.paused ORDER BY t.priority DESC, t.run_at, t.id LIMIT 3 FOR UPDATE OF t; }
step s5l	{ SELECT t.id, t.attempts, q.name FROM snt t JOIN snq q ON q.id = t.queue_id WHERE t.status = 'ready' AND NOT q.paused ORDER BY t.priority DESC, t.run_at, t.id LIMIT 3 FOR UPDATE OF t; }

session s4
setup		{ SET enable_hashjoin = off; SET enable_mergejoin = off; SET enable_memoize = off; SET enable_indexscan = off; SET enable_bitmapscan = off; }
step s4x	{ EXPLAIN (COSTS OFF) SELECT t.id, q.name FROM snt t JOIN snq q ON q.id = t.queue_id WHERE t.status = 'ready' AND NOT q.paused ORDER BY t.priority DESC, t.run_at, t.id LIMIT 3 FOR UPDATE OF t; }
step s4l	{ SELECT t.id, t.attempts, q.name FROM snt t JOIN snq q ON q.id = t.queue_id WHERE t.status = 'ready' AND NOT q.paused ORDER BY t.priority DESC, t.run_at, t.id LIMIT 3 FOR UPDATE OF t; }

# the claim order: ids 3 (queue 4), 11 (queue 2), 15 (queue 1), 19 (queue 5), 23 (queue 4); 7 and 27 are in the paused queue 3
permutation s2x s1a s2l s1c
permutation s1s s2l s1c
permutation s1p s2l s1c
permutation s1q s2l s1c
permutation s1n s1a s2l s1c
permutation s1d s2l s1c
permutation s1s s2o s1c
permutation s1l s2sk s1c
permutation s1l s2nw s1c
permutation s3x s1a s3l s1c
permutation s1q s3l s1c
permutation s1n s1a s3l s1c
permutation s5x s1a s5l s1c
permutation s1q s5l s1c
permutation s1n s1a s5l s1c
permutation s4x s1a s4l s1c
permutation s1q s4l s1c
permutation s1s s4l s1c

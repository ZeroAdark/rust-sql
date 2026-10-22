# D3560 REFNEST-LEFT: Django's claim over a nullable FK, Limit -> LockRows -> Nested Loop Left Join (the task's claim-index
# scan, FOR UPDATE OF t; the queue's scan, REFERENCE). Over a newer version of t (READ COMMITTED) PostgreSQL's EvalPlanQual
# reruns the join over the test tuple: t's newest version through its index quals, the nest's parameter from it, q
# refetched at the row's junk ctid (the version the join read; none for a null-extended row) and rechecked under that
# parameter, then the join qual; the nest null-extends the row when the inner yields nothing. So a task whose queue_id s1
# set to NULL, or moved to another queue, comes back null-extended (the refetched old queue row fails the new parameter;
# the new queue is never read), a null-queue task comes back null-extended, a renamed queue keeps the name the join read;
# a task no longer ready is skipped and the Limit takes the next row. SKIP LOCKED, NOWAIT. s2: a Memoize over the pkey
# probe (enable_material off); s3: a Materialize over the queue's scan; s4: the probe alone; s5: a filter in the ON
# clause (a paused queue null-extends). Expected output is PostgreSQL 18.6's.
setup
{
  CREATE TABLE rlq (id int PRIMARY KEY, name text NOT NULL, paused bool NOT NULL DEFAULT false);
  CREATE TABLE rlt (id bigint PRIMARY KEY, queue_id int, status text NOT NULL, priority int NOT NULL, run_at timestamptz NOT NULL, attempts int NOT NULL DEFAULT 0);
  CREATE INDEX rlt_claim ON rlt (status, priority DESC, run_at) WHERE status = 'ready';
  INSERT INTO rlq SELECT g, 'q' || g, g = 3 FROM generate_series(1, 5) g;
  INSERT INTO rlt SELECT g, CASE WHEN g % 9 = 0 THEN NULL ELSE g % 5 + 1 END, CASE WHEN g % 6 = 0 THEN 'done' ELSE 'ready' END, g % 4, timestamptz '2026-01-01' + g * interval '1 minute', g % 3 FROM generate_series(1, 400) g;
  ANALYZE rlq, rlt;
}
teardown { DROP TABLE rlt, rlq; }

session s1
setup		{ BEGIN; }
step s1a	{ UPDATE rlt SET attempts = attempts + 10 WHERE id = 3; }
step s1s	{ UPDATE rlt SET status = 'done' WHERE id = 3; }
step s1z	{ UPDATE rlt SET queue_id = NULL WHERE id = 3; }
step s1m	{ UPDATE rlt SET queue_id = 2 WHERE id = 3; }
step s1p	{ UPDATE rlt SET queue_id = 3 WHERE id = 3; }
step s1n	{ UPDATE rlt SET attempts = attempts + 10 WHERE id = 27; }
step s1r	{ UPDATE rlq SET name = 'renamed' WHERE id = 4; UPDATE rlt SET attempts = attempts + 10 WHERE id = 3; }
step s1l	{ SELECT id FROM rlt WHERE id = 3 FOR UPDATE; }
step s1c	{ COMMIT; }
teardown	{ ROLLBACK; }

session s2
setup		{ SET enable_material = off; }
step s2x	{ EXPLAIN (COSTS OFF) SELECT t.id, t.queue_id, t.attempts, q.name FROM rlt t LEFT JOIN rlq q ON q.id = t.queue_id WHERE t.status = 'ready' ORDER BY t.priority DESC, t.run_at LIMIT 7 FOR UPDATE OF t; }
step s2l	{ SELECT t.id, t.queue_id, t.attempts, q.name FROM rlt t LEFT JOIN rlq q ON q.id = t.queue_id WHERE t.status = 'ready' ORDER BY t.priority DESC, t.run_at LIMIT 7 FOR UPDATE OF t; }
step s2sk	{ SELECT t.id, q.name FROM rlt t LEFT JOIN rlq q ON q.id = t.queue_id WHERE t.status = 'ready' ORDER BY t.priority DESC, t.run_at LIMIT 3 FOR UPDATE OF t SKIP LOCKED; }
step s2nw	{ SELECT t.id, q.name FROM rlt t LEFT JOIN rlq q ON q.id = t.queue_id WHERE t.status = 'ready' ORDER BY t.priority DESC, t.run_at LIMIT 3 FOR UPDATE OF t NOWAIT; }

session s3
setup		{ SET enable_memoize = off; }
step s3x	{ EXPLAIN (COSTS OFF) SELECT t.id, t.queue_id, t.attempts, q.name FROM rlt t LEFT JOIN rlq q ON q.id = t.queue_id WHERE t.status = 'ready' ORDER BY t.priority DESC, t.run_at LIMIT 7 FOR UPDATE OF t; }
step s3l	{ SELECT t.id, t.queue_id, t.attempts, q.name FROM rlt t LEFT JOIN rlq q ON q.id = t.queue_id WHERE t.status = 'ready' ORDER BY t.priority DESC, t.run_at LIMIT 7 FOR UPDATE OF t; }

session s4
setup		{ SET enable_memoize = off; SET enable_material = off; }
step s4x	{ EXPLAIN (COSTS OFF) SELECT t.id, t.queue_id, t.attempts, q.name FROM rlt t LEFT JOIN rlq q ON q.id = t.queue_id WHERE t.status = 'ready' ORDER BY t.priority DESC, t.run_at LIMIT 7 FOR UPDATE OF t; }
step s4l	{ SELECT t.id, t.queue_id, t.attempts, q.name FROM rlt t LEFT JOIN rlq q ON q.id = t.queue_id WHERE t.status = 'ready' ORDER BY t.priority DESC, t.run_at LIMIT 7 FOR UPDATE OF t; }

session s5
setup		{ SET enable_material = off; }
step s5l	{ SELECT t.id, t.queue_id, q.name FROM rlt t LEFT JOIN rlq q ON q.id = t.queue_id AND NOT q.paused WHERE t.status = 'ready' ORDER BY t.priority DESC, t.run_at LIMIT 4 FOR UPDATE OF t; }

# the claim order (priority 3, then run_at): ids 3 (queue 4), 7 (queue 3), 11 (queue 2), 15 (queue 1), 19 (queue 5), 23 (queue 4), 27 (no queue), ..
permutation s2x s1a s2l s1c
permutation s1s s2l s1c
permutation s1z s2l s1c
permutation s1m s2l s1c
permutation s1n s2l s1c
permutation s1r s2l s1c
permutation s1l s2sk s1c
permutation s1l s2nw s1c
permutation s3x s1a s3l s1c
permutation s1m s3l s1c
permutation s1z s3l s1c
permutation s4x s1a s4l s1c
permutation s1m s4l s1c
permutation s1p s5l s1c
permutation s1a s5l s1c

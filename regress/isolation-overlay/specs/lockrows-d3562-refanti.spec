# D3562 REFANTI: a claim skipping the tasks a side table names, Limit -> LockRows -> Nested Loop Anti Join (the task's
# claim-index scan; the side table's scan, REFERENCE-marked, its junk ctid NULL above the anti join). Over a newer version
# of the task (READ COMMITTED) PostgreSQL's EvalPlanQual gives the side table no test tuple (a NULL ctid: its scan
# empty), so the newest version passes the anti join on its own quals alone: a task updated and named in the side table
# by the same transaction is still claimed (NOT EXISTS is not rechecked); a task no longer ready is skipped and the Limit
# takes the next row. SKIP LOCKED, NOWAIT. s2: the anti nest over the side table's index; s3: a Hash Anti Join; s4: the
# nest over a Materialize of an unindexed side table. Expected output is PostgreSQL 18.6's.
setup
{
  CREATE TABLE rat (id bigint PRIMARY KEY, status text NOT NULL, priority int NOT NULL, run_at timestamptz NOT NULL, attempts int NOT NULL DEFAULT 0);
  CREATE INDEX rat_claim ON rat (status, priority DESC, run_at) WHERE status = 'ready';
  CREATE TABLE ral (task_id bigint NOT NULL, owner text NOT NULL);
  CREATE INDEX ral_task ON ral (task_id);
  CREATE TABLE rah (task_id bigint NOT NULL, reason text);
  INSERT INTO rat SELECT g, CASE WHEN g % 6 = 0 THEN 'done' ELSE 'ready' END, g % 4, timestamptz '2026-01-01' + g * interval '1 minute', g % 3 FROM generate_series(1, 400) g;
  INSERT INTO ral SELECT g, 'w' || (g % 3) FROM generate_series(1, 400) g WHERE g % 5 = 0;
  INSERT INTO rah SELECT g, 'hold' FROM generate_series(1, 400) g WHERE g % 7 = 0;
  ANALYZE rat, ral, rah;
}
teardown { DROP TABLE rat, ral, rah; }

session s1
setup		{ BEGIN; }
step s1a	{ UPDATE rat SET attempts = attempts + 10 WHERE id = 3; }
step s1s	{ UPDATE rat SET status = 'done' WHERE id = 3; }
step s1l	{ INSERT INTO ral VALUES (3, 'x'); UPDATE rat SET attempts = attempts + 10 WHERE id = 3; }
step s1h	{ INSERT INTO rah VALUES (3, 'x'); UPDATE rat SET attempts = attempts + 10 WHERE id = 3; }
step s1u	{ DELETE FROM ral WHERE task_id = 15; UPDATE rat SET attempts = attempts + 10 WHERE id = 3; }
step s1k	{ SELECT id FROM rat WHERE id = 3 FOR UPDATE; }
step s1c	{ COMMIT; }
teardown	{ ROLLBACK; }

session s2
step s2x	{ EXPLAIN (COSTS OFF) SELECT t.id, t.attempts FROM rat t WHERE t.status = 'ready' AND NOT EXISTS (SELECT 1 FROM ral l WHERE l.task_id = t.id) ORDER BY t.priority DESC, t.run_at LIMIT 5 FOR UPDATE; }
step s2l	{ SELECT t.id, t.attempts FROM rat t WHERE t.status = 'ready' AND NOT EXISTS (SELECT 1 FROM ral l WHERE l.task_id = t.id) ORDER BY t.priority DESC, t.run_at LIMIT 5 FOR UPDATE; }
step s2sk	{ SELECT t.id FROM rat t WHERE t.status = 'ready' AND NOT EXISTS (SELECT 1 FROM ral l WHERE l.task_id = t.id) ORDER BY t.priority DESC, t.run_at LIMIT 3 FOR UPDATE SKIP LOCKED; }
step s2nw	{ SELECT t.id FROM rat t WHERE t.status = 'ready' AND NOT EXISTS (SELECT 1 FROM ral l WHERE l.task_id = t.id) ORDER BY t.priority DESC, t.run_at LIMIT 3 FOR UPDATE NOWAIT; }

session s3
setup		{ SET enable_nestloop = off; SET enable_mergejoin = off; }
step s3x	{ EXPLAIN (COSTS OFF) SELECT t.id, t.attempts FROM rat t WHERE t.status = 'ready' AND NOT EXISTS (SELECT 1 FROM ral l WHERE l.task_id = t.id) ORDER BY t.priority DESC, t.run_at LIMIT 5 FOR UPDATE; }
step s3l	{ SELECT t.id, t.attempts FROM rat t WHERE t.status = 'ready' AND NOT EXISTS (SELECT 1 FROM ral l WHERE l.task_id = t.id) ORDER BY t.priority DESC, t.run_at LIMIT 5 FOR UPDATE; }

session s4
setup		{ SET enable_hashjoin = off; SET enable_mergejoin = off; }
step s4x	{ EXPLAIN (COSTS OFF) SELECT t.id, t.attempts FROM rat t WHERE t.status = 'ready' AND NOT EXISTS (SELECT 1 FROM rah h WHERE h.task_id = t.id) ORDER BY t.priority DESC, t.run_at LIMIT 5 FOR UPDATE; }
step s4l	{ SELECT t.id, t.attempts FROM rat t WHERE t.status = 'ready' AND NOT EXISTS (SELECT 1 FROM rah h WHERE h.task_id = t.id) ORDER BY t.priority DESC, t.run_at LIMIT 5 FOR UPDATE; }

# the claim order (priority 3, then run_at): ids 3, 7, 11, 15 (named in ral), 19, 23, 27, ..; rah names 7, 21, 35, ..
permutation s2x s1a s2l s1c
permutation s1s s2l s1c
permutation s1l s2l s1c
permutation s1u s2l s1c
permutation s1k s2sk s1c
permutation s1k s2nw s1c
permutation s3x s1a s3l s1c
permutation s1l s3l s1c
permutation s1s s3l s1c
permutation s4x s1a s4l s1c
permutation s1h s4l s1c

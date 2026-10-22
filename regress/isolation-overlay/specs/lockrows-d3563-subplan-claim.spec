# D3563: a limited claim with a scalar subquery column (Limit -> LockRows -> the task's scan, SubPlan 1 over the queue in
# its target list). Over a newer task version (READ COMMITTED) PostgreSQL's EvalPlanQual reruns the scan's quals and
# projection over it, the SubPlan again with its new queue_id under the statement's snapshot: a task moved to another
# queue meanwhile gets that queue's name as the snapshot saw it, a queue renamed meanwhile keeps its old name, a task
# whose status changed fails the qual; SKIP LOCKED skips a locked task. Then sorted off any index (Limit -> LockRows ->
# Result (the SubPlan) -> Sort -> the task's scan). The expected output is PostgreSQL 18.6's.
setup
{
  CREATE TABLE sq (id int PRIMARY KEY, name text NOT NULL);
  CREATE TABLE st (id int PRIMARY KEY, queue_id int NOT NULL, status text NOT NULL, note text);
  INSERT INTO sq SELECT g, 'queue' || g FROM generate_series(1, 4) g;
  INSERT INTO st SELECT g, g % 4 + 1, 'ready', 'n' || g FROM generate_series(1, 12) g;
  ANALYZE sq, st;
}
teardown { DROP TABLE st, sq; }

session s1
setup		{ BEGIN; }
step s1u	{ UPDATE st SET note = 'changed' WHERE id = 1; }
step s1m	{ UPDATE st SET note = 'moved', queue_id = 4 WHERE id = 1; }
step s1r	{ UPDATE sq SET name = 'renamed' WHERE id = 4; UPDATE st SET note = 'moved', queue_id = 4 WHERE id = 1; }
step s1s	{ UPDATE st SET status = 'done' WHERE id = 1; }
step s1k	{ SELECT id FROM st WHERE id = 1 FOR UPDATE; }
step s1c	{ COMMIT; }

session s2
setup		{ SET enable_sort = off; SET enable_seqscan = off; SET enable_bitmapscan = off; }
step s2xs	{ EXPLAIN (COSTS OFF) SELECT t.id, t.note, (SELECT q.name FROM sq q WHERE q.id = t.queue_id) AS qname FROM st t WHERE t.status = 'ready' ORDER BY t.id + 0 LIMIT 2 FOR UPDATE; }
step s2x	{ EXPLAIN (COSTS OFF) SELECT t.id, t.note, (SELECT q.name FROM sq q WHERE q.id = t.queue_id) AS qname FROM st t WHERE t.status = 'ready' ORDER BY t.id LIMIT 2 FOR UPDATE; }
step s2a	{ SELECT t.id, t.note, (SELECT q.name FROM sq q WHERE q.id = t.queue_id) AS qname FROM st t WHERE t.status = 'ready' ORDER BY t.id LIMIT 2 FOR UPDATE; }
step s2srt	{ SELECT t.id, t.note, (SELECT q.name FROM sq q WHERE q.id = t.queue_id) AS qname FROM st t WHERE t.status = 'ready' ORDER BY t.id + 0 LIMIT 2 FOR UPDATE; }
step s2sk	{ SELECT t.id, t.note, (SELECT q.name FROM sq q WHERE q.id = t.queue_id) AS qname FROM st t WHERE t.status = 'ready' ORDER BY t.id LIMIT 2 FOR UPDATE SKIP LOCKED; }

permutation s2x s1u s2a s1c
permutation s1m s2a s1c
permutation s1r s2a s1c
permutation s1s s2a s1c
permutation s1k s2sk s1c
permutation s2xs s1m s2srt s1c
permutation s1r s2srt s1c

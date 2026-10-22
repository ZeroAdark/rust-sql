# D3596: a limited claim over a Sort of a hash join and a hash LEFT join of REFERENCE relations (LockRows -> Sort ->
# Hash Left Join (Hash Join (the task's scan; Hash <- the queue's); Hash <- the lock's)). Over a newer task version
# (READ COMMITTED) PostgreSQL's EvalPlanQual reruns the Sort's subtree over it and the queue and lock rows refetched at
# their junk ctids (the versions the joins read; none for a null-extended lock row): a task updated meanwhile is claimed
# with its new values, one moved to another queue fails the join, one whose status changed fails the qual, a
# null-extended row stays null-extended though a lock row was inserted meanwhile, a changed lock row's old version is
# the one read; SKIP LOCKED skips a locked task. Then one LEFT join alone. The expected output is PostgreSQL 18.6's.
setup
{
  CREATE TABLE hq (id int PRIMARY KEY, name text NOT NULL, paused bool NOT NULL DEFAULT false);
  CREATE TABLE ht (id int PRIMARY KEY, queue_id int NOT NULL, status text NOT NULL, note text, run_at int NOT NULL);
  CREATE TABLE hl (id serial PRIMARY KEY, task_id int UNIQUE, owner text NOT NULL);
  INSERT INTO hq SELECT g, 'queue' || g, g = 3 FROM generate_series(1, 4) g;
  INSERT INTO ht SELECT g, g % 4 + 1, 'ready', 'n' || g, CASE WHEN g IN (1, 4) THEN g ELSE 100 + g END FROM generate_series(1, 40) g;
  INSERT INTO hl (task_id, owner) SELECT g, 'w' || g FROM generate_series(1, 12, 2) g;
  ANALYZE hq, ht, hl;
}
teardown { DROP TABLE hl, ht, hq; }

session s1
setup		{ BEGIN; }
step s1u	{ UPDATE ht SET note = 'changed' WHERE id = 1; }
step s1q	{ UPDATE ht SET note = 'moved', queue_id = 3 WHERE id = 1; }
step s1s	{ UPDATE ht SET status = 'done' WHERE id = 1; }
step s1n	{ UPDATE ht SET note = 'held' WHERE id = 4; INSERT INTO hl (task_id, owner) VALUES (4, 'late'); }
step s1l	{ UPDATE ht SET note = 'relocked' WHERE id = 1; UPDATE hl SET owner = 'other' WHERE task_id = 1; }
step s1k	{ SELECT id FROM ht WHERE id = 1 FOR UPDATE; }
step s1c	{ COMMIT; }

session s2
setup		{ SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_indexscan = off; SET enable_bitmapscan = off; }
step s2x	{ EXPLAIN (COSTS OFF) SELECT t.id, t.note, q.name, l.owner FROM ht t JOIN hq q ON q.id = t.queue_id LEFT JOIN hl l ON l.task_id = t.id WHERE t.status = 'ready' AND NOT q.paused ORDER BY t.run_at LIMIT 2 FOR UPDATE OF t; }
step s2a	{ SELECT t.id, t.note, q.name, l.owner FROM ht t JOIN hq q ON q.id = t.queue_id LEFT JOIN hl l ON l.task_id = t.id WHERE t.status = 'ready' AND NOT q.paused ORDER BY t.run_at LIMIT 2 FOR UPDATE OF t; }
step s2sk	{ SELECT t.id, t.note, q.name, l.owner FROM ht t JOIN hq q ON q.id = t.queue_id LEFT JOIN hl l ON l.task_id = t.id WHERE t.status = 'ready' AND NOT q.paused ORDER BY t.run_at LIMIT 2 FOR UPDATE OF t SKIP LOCKED; }
step s2o	{ SELECT t.id, t.note, l.owner FROM ht t LEFT JOIN hl l ON l.task_id = t.id WHERE t.status = 'ready' ORDER BY t.run_at LIMIT 2 FOR UPDATE OF t; }

permutation s2x s1u s2a s1c
permutation s1q s2a s1c
permutation s1s s2a s1c
permutation s1n s2a s1c
permutation s1l s2a s1c
permutation s1k s2sk s1c
permutation s1n s2o s1c
permutation s1l s2o s1c

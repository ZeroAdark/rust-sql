# D3564 REFNEST-CHAIN: a limited claim over an INNER then a LEFT nest of REFERENCE relations (LockRows -> Nested Loop
# Left Join (Nested Loop (the task's scan; the queue's); the lock's)). Over a newer task version (READ COMMITTED)
# PostgreSQL's EvalPlanQual reruns the joins over it and the queue and lock rows refetched at their junk ctids (the
# versions the joins read; none for a null-extended lock row): a task updated meanwhile is claimed with its new values,
# one moved to another queue fails the join (the queue row refetched is the one the join read), one whose status
# changed fails the qual, a null-extended row stays null-extended though a lock row was inserted meanwhile, a changed
# lock row's old version is the one read; SKIP LOCKED skips a locked task. Then the same with Materialize inners over
# the queue's and the lock's unparameterized scans. The expected output is PostgreSQL 18.6's.
setup
{
  CREATE TABLE rq (id int PRIMARY KEY, name text NOT NULL, paused bool NOT NULL DEFAULT false);
  CREATE TABLE rt (id int PRIMARY KEY, queue_id int NOT NULL, status text NOT NULL, priority int NOT NULL, note text);
  CREATE TABLE rl (id serial PRIMARY KEY, task_id int UNIQUE, owner text NOT NULL);
  INSERT INTO rq SELECT g, 'queue' || g, g = 3 FROM generate_series(1, 4) g;
  INSERT INTO rt SELECT g, g % 4 + 1, 'ready', g % 3, 'n' || g FROM generate_series(1, 12) g;
  INSERT INTO rl (task_id, owner) SELECT g, 'w' || g FROM generate_series(1, 12, 2) g;
  ANALYZE rq, rt, rl;
}
teardown { DROP TABLE rl, rt, rq; }

session s1
setup		{ BEGIN; }
step s1u	{ UPDATE rt SET note = 'changed', priority = 9 WHERE id = 1; }
step s1q	{ UPDATE rt SET note = 'moved', queue_id = 3 WHERE id = 1; }
step s1s	{ UPDATE rt SET status = 'done' WHERE id = 1; }
step s1n	{ UPDATE rt SET note = 'held' WHERE id = 4; INSERT INTO rl (task_id, owner) VALUES (4, 'late'); }
step s1l	{ UPDATE rt SET note = 'relocked' WHERE id = 1; UPDATE rl SET owner = 'other' WHERE task_id = 1; }
step s1k	{ SELECT id FROM rt WHERE id = 1 FOR UPDATE; }
step s1c	{ COMMIT; }
teardown	{ ROLLBACK; }

session s2
setup		{ SET enable_hashjoin = off; SET enable_mergejoin = off; SET enable_sort = off; SET enable_seqscan = off; SET enable_material = off; }
step s2x	{ EXPLAIN (COSTS OFF) SELECT t.id, t.note, q.name, l.owner FROM rt t JOIN rq q ON q.id = t.queue_id LEFT JOIN rl l ON l.task_id = t.id WHERE t.status = 'ready' AND NOT q.paused AND t.id IN (1, 4) ORDER BY t.id LIMIT 2 FOR UPDATE OF t; }
step s2a	{ SELECT t.id, t.note, q.name, l.owner FROM rt t JOIN rq q ON q.id = t.queue_id LEFT JOIN rl l ON l.task_id = t.id WHERE t.status = 'ready' AND NOT q.paused AND t.id IN (1, 4) ORDER BY t.id LIMIT 2 FOR UPDATE OF t; }
step s2mat	{ SET enable_material = on; }
step s2sk	{ SELECT t.id, t.note, q.name, l.owner FROM rt t JOIN rq q ON q.id = t.queue_id LEFT JOIN rl l ON l.task_id = t.id WHERE t.status = 'ready' AND NOT q.paused AND t.id IN (1, 4) ORDER BY t.id LIMIT 2 FOR UPDATE OF t SKIP LOCKED; }

permutation s2x s1u s2a s1c
permutation s1q s2a s1c
permutation s1s s2a s1c
permutation s1n s2a s1c
permutation s1l s2a s1c
permutation s1k s2sk s1c
permutation s2mat s2x s1u s2a s1c
permutation s2mat s1q s2a s1c
permutation s2mat s1n s2a s1c
permutation s2mat s1l s2a s1c

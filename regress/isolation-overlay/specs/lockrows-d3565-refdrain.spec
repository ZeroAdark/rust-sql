# D3565 REFDRAIN: a claim with no LIMIT skipping the tasks a side table names, nest and merge joins off: LockRows ->
# Sort -> Hash Right Anti Join, the side table's scan probing a hash of the task's scan, the rows out of the drain of
# the unmatched build rows (the side table REFERENCE-marked, its junk ctid NULL in a drained row). A task changes
# between the build and the lock (READ COMMITTED): PostgreSQL's EvalPlanQual gives the side table no test tuple, so the
# newest version of the task passes on the task scan's own quals alone: a task updated and named in the side table by
# the same transaction is still claimed (NOT EXISTS is not rechecked); a task no longer ready, or moved off priority 3,
# is skipped; a task still ready comes back with its new columns. SKIP LOCKED, NOWAIT. Expected output is PostgreSQL
# 18.6's.
setup
{
  CREATE TABLE rdt (id bigint PRIMARY KEY, status text NOT NULL, priority int NOT NULL, run_at timestamptz NOT NULL, attempts int NOT NULL DEFAULT 0);
  CREATE INDEX rdt_claim ON rdt (status, priority DESC, run_at) WHERE status = 'ready';
  CREATE TABLE rdl (task_id bigint NOT NULL, owner text NOT NULL);
  CREATE INDEX rdl_task ON rdl (task_id);
  INSERT INTO rdt SELECT g, CASE WHEN g % 6 = 0 THEN 'done' ELSE 'ready' END, g % 4, timestamptz '2026-01-01' + g * interval '1 minute', g % 3 FROM generate_series(1, 400) g;
  INSERT INTO rdl SELECT g, 'w' || (g % 3) FROM generate_series(1, 400) g WHERE g % 5 = 0 OR g = 3;
  ANALYZE rdt, rdl;
}
teardown { DROP TABLE rdt, rdl; }

session s1
setup		{ BEGIN; }
step s1a	{ UPDATE rdt SET attempts = attempts + 10 WHERE id = 7; }
step s1s	{ UPDATE rdt SET status = 'done' WHERE id = 7; }
step s1p	{ UPDATE rdt SET priority = 2 WHERE id = 7; }
step s1l	{ INSERT INTO rdl VALUES (7, 'x'); UPDATE rdt SET attempts = attempts + 10 WHERE id = 7; }
step s1u	{ DELETE FROM rdl WHERE task_id = 15; UPDATE rdt SET attempts = attempts + 10 WHERE id = 7; }
step s1k	{ SELECT id FROM rdt WHERE id = 7 FOR UPDATE; }
step s1c	{ COMMIT; }
teardown	{ ROLLBACK; }

session s2
setup		{ SET enable_nestloop = off; SET enable_mergejoin = off; }
step s2x	{ EXPLAIN (COSTS OFF) SELECT t.id, t.attempts FROM rdt t WHERE t.status = 'ready' AND t.priority = 3 AND t.id < 60 AND NOT EXISTS (SELECT 1 FROM rdl l WHERE l.task_id = t.id) ORDER BY t.priority DESC, t.run_at FOR UPDATE; }
step s2l	{ SELECT t.id, t.attempts FROM rdt t WHERE t.status = 'ready' AND t.priority = 3 AND t.id < 60 AND NOT EXISTS (SELECT 1 FROM rdl l WHERE l.task_id = t.id) ORDER BY t.priority DESC, t.run_at FOR UPDATE; }
step s2sk	{ SELECT t.id FROM rdt t WHERE t.status = 'ready' AND t.priority = 3 AND t.id < 60 AND NOT EXISTS (SELECT 1 FROM rdl l WHERE l.task_id = t.id) ORDER BY t.priority DESC, t.run_at FOR UPDATE SKIP LOCKED; }
step s2nw	{ SELECT t.id FROM rdt t WHERE t.status = 'ready' AND t.priority = 3 AND t.id < 60 AND NOT EXISTS (SELECT 1 FROM rdl l WHERE l.task_id = t.id) ORDER BY t.priority DESC, t.run_at FOR UPDATE NOWAIT; }

# the claim (priority 3, id < 60, ready, not in rdl, by run_at): ids 7, 11, 19, 23, 27, 31, 39, 43, 47, 51, 59
permutation s2x s1a s2l s1c
permutation s1s s2l s1c
permutation s1p s2l s1c
permutation s1l s2l s1c
permutation s1u s2l s1c
permutation s1k s2sk s1c
permutation s1k s2nw s1c

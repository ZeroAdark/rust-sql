# D3557: the claim over a join in the claiming UPDATE's own subquery: UPDATE rpt SET status = 'running', attempts =
# attempts + 1 WHERE id IN (SELECT t.id FROM rpt t JOIN rpq q ON q.id = t.queue_id WHERE t.status = 'ready' AND NOT q.paused
# ORDER BY t.priority DESC, t.run_at LIMIT 3 FOR UPDATE OF t) RETURNING .., the Update over a Hash Semi Join whose Hash
# reads Subquery Scan -> Limit -> LockRows -> Nested Loop (the task's claim-index scan; a Materialize over the queue's scan,
# REFERENCE). The subquery's LockRows rechecks under its own EvalPlanQual (the join over the task's newest version and the
# queue row refetched at its junk ctid) and hands the row on to the Limit and the Hash; the Update writes the rows the
# claim locked. A claimed row s1 changed is rechecked: still ready (its newest attempts updated again), no longer ready,
# moved to the paused queue, deleted (skipped, the Limit takes the next row); SKIP LOCKED skips a row s1 holds. s3: the
# Memoize inner; s4: the `id = (.. LIMIT 1 ..)` InitPlan form. Expected output is PostgreSQL 18.6's.
setup
{
  CREATE TABLE rpq (id int PRIMARY KEY, name text NOT NULL, paused bool NOT NULL DEFAULT false);
  CREATE TABLE rpt (id bigint PRIMARY KEY, queue_id int NOT NULL, status text NOT NULL, priority int NOT NULL, run_at timestamptz NOT NULL, attempts int NOT NULL DEFAULT 0);
  CREATE INDEX rpt_claim ON rpt (status, priority DESC, run_at) WHERE status = 'ready';
  INSERT INTO rpq SELECT g, 'q' || g, g = 3 FROM generate_series(1, 5) g;
  INSERT INTO rpt SELECT g, g % 5 + 1, CASE WHEN g % 6 = 0 THEN 'done' ELSE 'ready' END, g % 4, timestamptz '2026-01-01' + g * interval '1 minute', g % 3 FROM generate_series(1, 400) g;
  ANALYZE rpq, rpt;
}
teardown { DROP TABLE rpt, rpq; }

session s1
setup		{ BEGIN; }
step s1a	{ UPDATE rpt SET attempts = attempts + 10 WHERE id = 3; }
step s1s	{ UPDATE rpt SET status = 'done' WHERE id = 3; }
step s1q	{ UPDATE rpt SET queue_id = 3 WHERE id = 3; }
step s1d	{ DELETE FROM rpt WHERE id = 11; }
step s1l	{ SELECT id FROM rpt WHERE id = 3 FOR UPDATE; }
step s1c	{ COMMIT; }
teardown	{ ROLLBACK; }

session s2
step s2x	{ EXPLAIN (COSTS OFF) UPDATE rpt SET status = 'running', attempts = attempts + 1 WHERE id IN (SELECT t.id FROM rpt t JOIN rpq q ON q.id = t.queue_id WHERE t.status = 'ready' AND NOT q.paused ORDER BY t.priority DESC, t.run_at LIMIT 3 FOR UPDATE OF t) RETURNING id, attempts; }
step s2u	{ UPDATE rpt SET status = 'running', attempts = attempts + 1 WHERE id IN (SELECT t.id FROM rpt t JOIN rpq q ON q.id = t.queue_id WHERE t.status = 'ready' AND NOT q.paused ORDER BY t.priority DESC, t.run_at LIMIT 3 FOR UPDATE OF t) RETURNING id, attempts; }
step s2sk	{ UPDATE rpt SET status = 'running' WHERE id IN (SELECT t.id FROM rpt t JOIN rpq q ON q.id = t.queue_id WHERE t.status = 'ready' AND NOT q.paused ORDER BY t.priority DESC, t.run_at LIMIT 3 FOR UPDATE OF t SKIP LOCKED) RETURNING id; }

session s3
setup		{ SET enable_material = off; }
step s3x	{ EXPLAIN (COSTS OFF) UPDATE rpt SET status = 'running', attempts = attempts + 1 WHERE id IN (SELECT t.id FROM rpt t JOIN rpq q ON q.id = t.queue_id WHERE t.status = 'ready' AND NOT q.paused ORDER BY t.priority DESC, t.run_at LIMIT 3 FOR UPDATE OF t) RETURNING id, attempts; }
step s3u	{ UPDATE rpt SET status = 'running', attempts = attempts + 1 WHERE id IN (SELECT t.id FROM rpt t JOIN rpq q ON q.id = t.queue_id WHERE t.status = 'ready' AND NOT q.paused ORDER BY t.priority DESC, t.run_at LIMIT 3 FOR UPDATE OF t) RETURNING id, attempts; }

session s4
step s4x	{ EXPLAIN (COSTS OFF) UPDATE rpt SET status = 'running', attempts = attempts + 1 WHERE id = (SELECT t.id FROM rpt t JOIN rpq q ON q.id = t.queue_id WHERE t.status = 'ready' AND NOT q.paused ORDER BY t.priority DESC, t.run_at LIMIT 1 FOR UPDATE OF t) RETURNING id, attempts; }
step s4u	{ UPDATE rpt SET status = 'running', attempts = attempts + 1 WHERE id = (SELECT t.id FROM rpt t JOIN rpq q ON q.id = t.queue_id WHERE t.status = 'ready' AND NOT q.paused ORDER BY t.priority DESC, t.run_at LIMIT 1 FOR UPDATE OF t) RETURNING id, attempts; }

# the claim order: ids 3 (queue 4), 11 (queue 2), 15 (queue 1), 19 (queue 5); 7 is in the paused queue 3
permutation s2x s1a s2u s1c
permutation s1s s2u s1c
permutation s1q s2u s1c
permutation s1d s2u s1c
permutation s1l s2sk s1c
permutation s3x s1a s3u s1c
permutation s1q s3u s1c
permutation s4x s1a s4u s1c
permutation s1s s4u s1c
permutation s1q s4u s1c

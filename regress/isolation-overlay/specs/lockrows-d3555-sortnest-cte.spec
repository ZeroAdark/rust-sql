# D3555: D3554 SORTNEST's claim in a CTE the claiming UPDATE reads: WITH c AS (SELECT t.id FROM cnt t JOIN cnq q ON q.id =
# t.queue_id WHERE .. ORDER BY t.priority DESC, t.run_at, t.id LIMIT 3 FOR UPDATE OF t) UPDATE cnt SET status = 'running',
# attempts = attempts + 1 FROM c WHERE cnt.id = c.id RETURNING ... The CTE's LockRows rechecks under its own EvalPlanQual
# (the Sort's subtree over the test tuple and the queue row refetched at its junk ctid) and hands the row to the Limit;
# the UPDATE then writes the rows the CTE locked (no concurrent change can reach them). A claimed row s1 changed is
# rechecked: still ready (its newest attempts updated again), no longer ready or moved to another queue (skipped, the
# Limit takes the next row), deleted (skipped); SKIP LOCKED skips a row s1 holds. s3: the Materialize inner. Expected
# output is PostgreSQL 18.6's.
setup
{
  CREATE TABLE cnq (id int PRIMARY KEY, name text NOT NULL, paused bool NOT NULL DEFAULT false);
  CREATE TABLE cnt (id bigint PRIMARY KEY, queue_id int NOT NULL, status text NOT NULL, priority int NOT NULL, run_at timestamptz NOT NULL, attempts int NOT NULL DEFAULT 0);
  CREATE INDEX cnt_claim ON cnt (status, priority DESC, run_at) WHERE status = 'ready';
  INSERT INTO cnq SELECT g, 'q' || g, g = 3 FROM generate_series(1, 5) g;
  INSERT INTO cnt SELECT g, g % 5 + 1, CASE WHEN g % 6 = 0 THEN 'done' ELSE 'ready' END, g % 4, timestamptz '2026-01-01' + g * interval '1 minute', g % 3 FROM generate_series(1, 400) g;
  ANALYZE cnq, cnt;
}
teardown { DROP TABLE cnt, cnq; }

session s1
setup		{ BEGIN; }
step s1a	{ UPDATE cnt SET attempts = attempts + 10 WHERE id = 3; }
step s1s	{ UPDATE cnt SET status = 'done' WHERE id = 3; }
step s1q	{ UPDATE cnt SET queue_id = 3 WHERE id = 3; }
step s1d	{ DELETE FROM cnt WHERE id = 11; }
step s1l	{ SELECT id FROM cnt WHERE id = 3 FOR UPDATE; }
step s1c	{ COMMIT; }
teardown	{ ROLLBACK; }

session s2
setup		{ SET enable_material = off; }
step s2x	{ EXPLAIN (COSTS OFF) WITH c AS (SELECT t.id FROM cnt t JOIN cnq q ON q.id = t.queue_id WHERE t.status = 'ready' AND NOT q.paused ORDER BY t.priority DESC, t.run_at, t.id LIMIT 3 FOR UPDATE OF t) UPDATE cnt SET status = 'running', attempts = attempts + 1 FROM c WHERE cnt.id = c.id RETURNING cnt.id, cnt.attempts; }
step s2u	{ WITH c AS (SELECT t.id FROM cnt t JOIN cnq q ON q.id = t.queue_id WHERE t.status = 'ready' AND NOT q.paused ORDER BY t.priority DESC, t.run_at, t.id LIMIT 3 FOR UPDATE OF t) UPDATE cnt SET status = 'running', attempts = attempts + 1 FROM c WHERE cnt.id = c.id RETURNING cnt.id, cnt.attempts; }
step s2sk	{ WITH c AS (SELECT t.id FROM cnt t JOIN cnq q ON q.id = t.queue_id WHERE t.status = 'ready' AND NOT q.paused ORDER BY t.priority DESC, t.run_at, t.id LIMIT 3 FOR UPDATE OF t SKIP LOCKED) UPDATE cnt SET status = 'running' FROM c WHERE cnt.id = c.id RETURNING cnt.id; }

session s3
setup		{ SET enable_memoize = off; }
step s3x	{ EXPLAIN (COSTS OFF) WITH c AS (SELECT t.id FROM cnt t JOIN cnq q ON q.id = t.queue_id WHERE t.status = 'ready' AND NOT q.paused ORDER BY t.priority DESC, t.run_at, t.id LIMIT 3 FOR UPDATE OF t) UPDATE cnt SET status = 'running', attempts = attempts + 1 FROM c WHERE cnt.id = c.id RETURNING cnt.id, cnt.attempts; }
step s3u	{ WITH c AS (SELECT t.id FROM cnt t JOIN cnq q ON q.id = t.queue_id WHERE t.status = 'ready' AND NOT q.paused ORDER BY t.priority DESC, t.run_at, t.id LIMIT 3 FOR UPDATE OF t) UPDATE cnt SET status = 'running', attempts = attempts + 1 FROM c WHERE cnt.id = c.id RETURNING cnt.id, cnt.attempts; }

# the claim order: ids 3 (queue 4), 11 (queue 2), 15 (queue 1), 19 (queue 5); 7 is in the paused queue 3
permutation s2x s1a s2u s1c
permutation s1s s2u s1c
permutation s1q s2u s1c
permutation s1d s2u s1c
permutation s1l s2sk s1c
permutation s3x s1a s3u s1c
permutation s1q s3u s1c
permutation s1s s3u s1c

# D3556 PAIRSPOOL: ActiveRecord's lock over a join, both relations locked: Limit -> LockRows -> Nested Loop (Join Filter
# q.id = t.queue_id; the outer t's claim-index scan; the inner a Materialize over q's seq scan). ExecLockRows locks t's row,
# then q's; when either lock traversed to a newer version EvalPlanQual reruns the join over both test tuples (each mark's
# newest version where its lock traversed, else the version it locked): t's index quals over t's, a fresh Materialize over
# q's (its filter name = 'q2'), then the join filter; LockRows hands the row to the Limit, which reads one more row for
# each the recheck drops. The forms: t kept (newest), t no longer ready, t moved to another queue (the join filter fails),
# t deleted, the queue renamed off the filter (every row of it dropped, the t locks kept), the queue changed but still
# passing (its newest columns returned), SKIP LOCKED on t and on q, NOWAIT, OFFSET, and mixed strengths (s3).
# Expected output is PostgreSQL 18.6's.
setup
{
  CREATE TABLE spq (id int PRIMARY KEY, name text NOT NULL, paused bool NOT NULL DEFAULT false, max_attempts int NOT NULL DEFAULT 5);
  CREATE TABLE spt (id bigint PRIMARY KEY, queue_id int NOT NULL, status text NOT NULL, priority int NOT NULL, run_at timestamptz NOT NULL, attempts int NOT NULL DEFAULT 0);
  CREATE INDEX spt_claim ON spt (status, priority DESC, run_at) WHERE status = 'ready';
  INSERT INTO spq SELECT g, 'q' || g, g = 3 FROM generate_series(1, 5) g;
  INSERT INTO spt SELECT g, g % 5 + 1, CASE WHEN g % 6 = 0 THEN 'done' ELSE 'ready' END, g % 4, timestamptz '2026-01-01' + g * interval '1 minute', g % 3 FROM generate_series(1, 400) g;
  ANALYZE spq, spt;
}
teardown { DROP TABLE spt, spq; }

session s1
setup		{ BEGIN; }
step s1a	{ UPDATE spt SET attempts = attempts + 1 WHERE id = 11; }
step s1s	{ UPDATE spt SET status = 'done' WHERE id = 11; }
step s1j	{ UPDATE spt SET queue_id = 3 WHERE id = 11; }
step s1d	{ DELETE FROM spt WHERE id = 11; }
step s1n	{ UPDATE spq SET name = 'q2x' WHERE id = 2; }
step s1r	{ UPDATE spq SET max_attempts = 9 WHERE id = 2; }
step s1l	{ SELECT id FROM spt WHERE id = 11 FOR UPDATE; }
step s1lq	{ SELECT id FROM spq WHERE id = 2 FOR UPDATE; }
step s1c	{ COMMIT; }
teardown	{ ROLLBACK; }

session s2
step s2x	{ EXPLAIN (COSTS OFF) SELECT t.id, t.attempts, q.name, q.max_attempts FROM spt t JOIN spq q ON q.id = t.queue_id WHERE t.status = 'ready' AND q.name = 'q2' ORDER BY t.priority DESC, t.run_at LIMIT 2 FOR UPDATE; }
step s2l	{ SELECT t.id, t.attempts, q.name, q.max_attempts FROM spt t JOIN spq q ON q.id = t.queue_id WHERE t.status = 'ready' AND q.name = 'q2' ORDER BY t.priority DESC, t.run_at LIMIT 2 FOR UPDATE; }
step s2o	{ SELECT t.id, q.name FROM spt t JOIN spq q ON q.id = t.queue_id WHERE t.status = 'ready' AND q.name = 'q2' ORDER BY t.priority DESC, t.run_at LIMIT 2 OFFSET 1 FOR UPDATE; }
step s2sk	{ SELECT t.id, q.name FROM spt t JOIN spq q ON q.id = t.queue_id WHERE t.status = 'ready' AND q.name = 'q2' ORDER BY t.priority DESC, t.run_at LIMIT 2 FOR UPDATE SKIP LOCKED; }
step s2nw	{ SELECT t.id, q.name FROM spt t JOIN spq q ON q.id = t.queue_id WHERE t.status = 'ready' AND q.name = 'q2' ORDER BY t.priority DESC, t.run_at LIMIT 2 FOR UPDATE NOWAIT; }

session s3
step s3l	{ SELECT t.id, t.attempts, q.max_attempts FROM spt t JOIN spq q ON q.id = t.queue_id WHERE t.status = 'ready' AND q.name = 'q2' ORDER BY t.priority DESC, t.run_at LIMIT 2 FOR NO KEY UPDATE OF t FOR SHARE OF q; }

# queue 2 holds ids 1, 11, 16, 21, 26, 31, ..; the claim order (priority 3 first): 11, 31, 51, ..
permutation s2x s1a s2l s1c
permutation s1s s2l s1c
permutation s1j s2l s1c
permutation s1d s2l s1c
permutation s1n s2l s1c
permutation s1r s2l s1c
permutation s1s s2o s1c
permutation s1l s2sk s1c
permutation s1lq s2sk s1c
permutation s1l s2nw s1c
permutation s1a s3l s1c
permutation s1r s3l s1c

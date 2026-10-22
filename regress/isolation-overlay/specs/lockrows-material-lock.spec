# D3718 LOCK-FILL (gexplain): a nest's plan Material whose child takes its own row locks, two LIMIT .. FOR UPDATE
# subqueries in a cross join: Nested Loop -> (Subquery s -> Limit -> LockRows -> Index Scan lmq; Materialize -> Subquery
# w -> Limit -> LockRows -> Index Scan lmk). ExecMaterial pulls its child whole at the nest's first outer row, so s2 locks
# the first job (3), then waits on worker 1 that s1 holds, before it reads the second job (7): s3 can still take job 7
# NOWAIT but not job 3. At s1's commit (READ COMMITTED) the inner LockRows' own EvalPlanQual rechecks the worker's scan
# alone: an updated worker is locked in its new version, a deleted one is skipped and the next one locked. A job that
# left the queue is skipped by the outer LockRows' recheck (its newest version stays locked) before the Material fills.
# An outer side that hands on no job (s2e) never fills the Material, so s2e does not wait on worker 1. Each s2 step
# reads back the rows its transaction locked (age(xmax) = 0). Expected output is PostgreSQL 18.6's.
setup
{
  CREATE TABLE lmq (id int PRIMARY KEY, state text NOT NULL);
  INSERT INTO lmq SELECT i, CASE WHEN i % 4 = 3 THEN 'ready' ELSE 'done' END FROM generate_series(1, 16) i;
  CREATE TABLE lmk (id int PRIMARY KEY, name text NOT NULL);
  INSERT INTO lmk SELECT i, 'worker-' || i FROM generate_series(1, 6) i;
  ANALYZE lmq, lmk;
}
teardown { DROP TABLE lmq, lmk; }

session s1
setup		{ BEGIN; }
step s1u	{ UPDATE lmk SET name = name || '+' WHERE id = 1; }
step s1d	{ DELETE FROM lmk WHERE id = 1; }
step s1l	{ SELECT id FROM lmk WHERE id = 1 FOR UPDATE; }
step s1q	{ UPDATE lmq SET state = 'done' WHERE id = 3; }
step s1c	{ COMMIT; }
teardown	{ ROLLBACK; }

session s2
setup		{ SET enable_seqscan = off; SET enable_bitmapscan = off; BEGIN; }
step s2x	{ EXPLAIN (COSTS OFF) SELECT * FROM (SELECT id FROM lmq WHERE state = 'ready' ORDER BY id LIMIT 3) s, (SELECT id, name FROM lmk ORDER BY id LIMIT 2) w FOR UPDATE; }
step s2l	{ SELECT * FROM (SELECT id FROM lmq WHERE state = 'ready' ORDER BY id LIMIT 3) s, (SELECT id, name FROM lmk ORDER BY id LIMIT 2) w FOR UPDATE; }
step s2e	{ SELECT * FROM (SELECT id FROM lmq WHERE state = 'ready' AND id <> id ORDER BY id LIMIT 3) s, (SELECT id, name FROM lmk ORDER BY id LIMIT 2) w FOR UPDATE; }
step s2r	{ SELECT 'q' AS t, id FROM lmq WHERE age(xmax) = 0 UNION ALL SELECT 'k', id FROM lmk WHERE age(xmax) = 0 ORDER BY 1, 2; }
step s2c	{ COMMIT; }
teardown	{ ROLLBACK; }

session s3
setup		{ BEGIN; }
step s3a	{ SELECT id FROM lmq WHERE id = 7 FOR UPDATE NOWAIT; }
step s3b	{ SELECT id FROM lmq WHERE id = 3 FOR UPDATE NOWAIT; }
step s3c	{ COMMIT; }
teardown	{ ROLLBACK; }

permutation s2x s1u s2l s1c s2r s2c
permutation s1d s2l s1c s2r s2c
permutation s1l s2l s3a s3b s3c s1c s2r s2c
permutation s1q s2l s1c s2r s2c
permutation s1l s2e s2r s2c s1c

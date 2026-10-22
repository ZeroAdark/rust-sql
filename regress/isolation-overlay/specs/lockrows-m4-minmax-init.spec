# lockrows M4 (lk-0061, lk-0069): a row lock over its own index scan keyed by a min() sublink's InitPlan (the planner's
# MinMaxAgg InitPlan nested inside it): SELECT .. WHERE id = (SELECT min(id) FROM job WHERE ..) FOR UPDATE [SKIP LOCKED].
# EvalPlanQualStart copies the sublink's value (attached at the LockRows) and never reads the nested one again; the
# index key forced it at the scan's first fetch. A concurrent update of the row (its newest version returned), one moving
# it out of the sublink's quals (still returned: the recheck does not rerun the sublink), a delete (no row), SKIP LOCKED
# and NOWAIT against a held lock. Expected output is the C oracle's (PostgreSQL 18, pgdg).
setup
{
  CREATE TABLE job (id int PRIMARY KEY, queue text NOT NULL, prio int NOT NULL, state text NOT NULL, attempts int NOT NULL DEFAULT 0);
  INSERT INTO job SELECT i, (ARRAY['mail', 'pay', 'report'])[1 + i % 3], (i * 7) % 10, CASE WHEN i % 5 = 0 THEN 'done' ELSE 'ready' END, 0 FROM generate_series(1, 300) i;
  ANALYZE job;
}

teardown
{
  DROP TABLE job;
}

session s1
step s1b	{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1u	{ UPDATE job SET attempts = attempts + 1 WHERE id = 1; }
step s1d	{ UPDATE job SET state = 'done', attempts = 9 WHERE id = 1; }
step s1x	{ DELETE FROM job WHERE id = 1; }
step s1l	{ SELECT id FROM job WHERE id = 1 FOR UPDATE; }
step s1c	{ COMMIT; }

session s2
setup		{ SET enable_seqscan = off; }
step s2b	{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s2x	{ EXPLAIN (COSTS OFF) SELECT id, queue, attempts FROM job WHERE id = (SELECT min(id) FROM job WHERE state = 'ready' AND queue = 'pay') FOR UPDATE; }
step s2m	{ SELECT id, queue, attempts FROM job WHERE id = (SELECT min(id) FROM job WHERE state = 'ready' AND queue = 'pay') FOR UPDATE; }
step s2s	{ SELECT id, queue, attempts FROM job WHERE id = (SELECT min(id) FROM job WHERE state = 'ready' AND queue = 'pay') FOR UPDATE SKIP LOCKED; }
step s2n	{ SELECT id, queue, attempts FROM job WHERE id = (SELECT min(id) FROM job WHERE state = 'ready' AND queue = 'pay') FOR UPDATE NOWAIT; }
step s2c	{ COMMIT; }

permutation s2b s2x s2m s2c
permutation s2b s1b s1u s2m s1c s2c
permutation s2b s1b s1d s2m s1c s2c
permutation s2b s1b s1x s2m s1c s2c
permutation s2b s1b s1l s2s s1c s2c
permutation s2b s1b s1l s2n s1c s2c

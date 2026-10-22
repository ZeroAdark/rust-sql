# D3379 SUBLOCK-LIMIT (gexplain): a LIMIT above the LockRows of a correlated EXISTS SubPlan, the next job of the same
# queue: Limit -> LockRows -> Index Scan (Index Cond id > j.id, Filter queue = j.queue; PostgreSQL keeps the LIMIT of a
# sublink holding row marks). For j = 1 (queue a) the candidates are 4, 7, 10. s2's LockRows waits on the row s1 holds;
# at s1's commit (READ COMMITTED) its own EvalPlanQual reruns the scan alone over the newest version: a version that left
# the queue (s1q) or a deleted row (s1d) is skipped and the scan's next candidate, 7, is locked; a version that still
# qualifies (s1a) is locked. SKIP LOCKED skips the row s1 holds. ExecLimit counts only rows LockRows hands on, so exactly
# one row is locked per outer row. s1j moves the target row j itself to another queue: the UPDATE's EvalPlanQual runs the
# SubPlan again over the newest j (queue b), which locks 2 (4 stays locked from the first pass). s3 is the plain SELECT
# form. Each s2 / s3 step reads back the rows its transaction locked (age(xmax) = 0). Expected output is PostgreSQL 18.6's.
setup
{
  CREATE TABLE lsq (id int PRIMARY KEY, queue text NOT NULL, attempts int NOT NULL DEFAULT 0);
  INSERT INTO lsq SELECT i, (ARRAY['a', 'b', 'c'])[1 + (i - 1) % 3], 0 FROM generate_series(1, 12) i;
  ANALYZE lsq;
}
teardown { DROP TABLE lsq; }

session s1
setup		{ BEGIN; }
step s1q	{ UPDATE lsq SET queue = 'z' WHERE id = 4; }
step s1a	{ UPDATE lsq SET attempts = attempts + 100 WHERE id = 4; }
step s1d	{ DELETE FROM lsq WHERE id = 4; }
step s1l	{ SELECT id FROM lsq WHERE id = 4 FOR UPDATE; }
step s1j	{ UPDATE lsq SET queue = 'b' WHERE id = 1; }
step s1c	{ COMMIT; }
teardown	{ ROLLBACK; }

session s2
setup		{ SET enable_seqscan = off; SET enable_bitmapscan = off; BEGIN; }
step s2x	{ EXPLAIN (COSTS OFF) UPDATE lsq j SET attempts = j.attempts + 1 WHERE j.id = 1 AND EXISTS (SELECT 1 FROM lsq k WHERE k.queue = j.queue AND k.id > j.id ORDER BY k.id LIMIT 1 FOR UPDATE) RETURNING j.id, j.queue; }
step s2u	{ UPDATE lsq j SET attempts = j.attempts + 1 WHERE j.id = 1 AND EXISTS (SELECT 1 FROM lsq k WHERE k.queue = j.queue AND k.id > j.id ORDER BY k.id LIMIT 1 FOR UPDATE) RETURNING j.id, j.queue; }
step s2sk	{ UPDATE lsq j SET attempts = j.attempts + 1 WHERE j.id = 1 AND EXISTS (SELECT 1 FROM lsq k WHERE k.queue = j.queue AND k.id > j.id ORDER BY k.id LIMIT 1 FOR UPDATE SKIP LOCKED) RETURNING j.id, j.queue; }
step s2r	{ SELECT id FROM lsq WHERE age(xmax) = 0 ORDER BY id; }
step s2c	{ COMMIT; }
teardown	{ ROLLBACK; }

session s3
setup		{ SET enable_seqscan = off; SET enable_bitmapscan = off; BEGIN; }
step s3l	{ SELECT j.id FROM lsq j WHERE j.id <= 3 AND EXISTS (SELECT 1 FROM lsq k WHERE k.queue = j.queue AND k.id > j.id ORDER BY k.id LIMIT 1 FOR UPDATE) ORDER BY j.id; }
step s3r	{ SELECT id FROM lsq WHERE age(xmax) = 0 ORDER BY id; }
step s3c	{ COMMIT; }
teardown	{ ROLLBACK; }

permutation s2x s1q s2u s1c s2r s2c
permutation s1a s2u s1c s2r s2c
permutation s1d s2u s1c s2r s2c
permutation s1l s2sk s2r s1c s2c
permutation s1j s2u s1c s2r s2c
permutation s1q s3l s1c s3r s3c
permutation s1a s3l s1c s3r s3c

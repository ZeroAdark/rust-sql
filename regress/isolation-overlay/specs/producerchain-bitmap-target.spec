# producerchain G3: an UPDATE / DELETE whose target's scan is a bitmap heap scan keyed by the outer row, the inner of an
# INNER nested loop over another relation's plain heap scan (`UPDATE bt ... FROM ba WHERE bt.ba_id = ba.id`, or after a
# LEFT nest over a third relation), meets a target row s1 changed. EvalPlanQual reruns the plan with the target's newest
# version: the bitmap scan's recheck condition is tested on it (BitmapHeapRecheck), so a version whose key moved off the
# outer row is skipped though its other quals pass, one that still matches is written (SET over its new values), a
# deleted one is skipped, and s1's changes to ba / bb are not seen.

setup
{
 CREATE TABLE ba (id int PRIMARY KEY, w int NOT NULL);
 INSERT INTO ba SELECT i, i % 3 FROM generate_series(1, 9) i;
 CREATE TABLE bb (id int PRIMARY KEY, ba_id int NOT NULL, tag text NOT NULL);
 INSERT INTO bb VALUES (1, 1, 'b1'), (4, 4, 'b4'), (8, 2, 'b8');
 CREATE TABLE bt (id int PRIMARY KEY, ba_id int NOT NULL, x int NOT NULL);
 INSERT INTO bt SELECT i, 1 + i % 9, 0 FROM generate_series(1, 900) i;
 CREATE INDEX bt_ba ON bt (ba_id);
 ANALYZE ba, bb, bt;
}

teardown
{
 DROP TABLE bt;
 DROP TABLE bb;
 DROP TABLE ba;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1x	{ UPDATE bt SET x = x + 1000 WHERE id IN (9, 12, 33); }
step s1k	{ UPDATE bt SET ba_id = 2 WHERE id = 18; }
step s1j	{ UPDATE bt SET ba_id = 4 WHERE id = 36; }
step s1d	{ DELETE FROM bt WHERE id = 27; }
step s1b	{ UPDATE bb SET tag = 'b4new' WHERE id = 4; }
step s1a	{ UPDATE ba SET w = 0 WHERE id = 7; }
step s1c	{ COMMIT; }

session s2
setup		{ SET enable_hashjoin = off; SET enable_mergejoin = off; SET enable_material = off; SET enable_memoize = off; SET enable_indexscan = off; SET max_parallel_workers_per_gather = 0; BEGIN ISOLATION LEVEL READ COMMITTED; }
step s2u	{ UPDATE bt SET x = x + 1 FROM ba WHERE bt.ba_id = ba.id AND ba.w = 1 AND bt.id < 60 RETURNING bt.id, bt.ba_id, bt.x, ba.id; }
step s2l	{ UPDATE bt SET x = x + 1 FROM ba LEFT JOIN bb ON bb.ba_id = ba.id WHERE bt.ba_id = ba.id AND ba.w = 1 AND bt.id < 60 RETURNING bt.id, bt.ba_id, bt.x, bb.tag, ba.id; }
step s2d	{ DELETE FROM bt USING ba WHERE bt.ba_id = ba.id AND ba.w = 1 AND bt.id < 60 RETURNING bt.id, ba.id; }
step s2c	{ COMMIT; }

session s3
setup		{ SET enable_hashjoin = off; SET enable_mergejoin = off; SET enable_material = off; SET enable_memoize = off; SET enable_indexscan = off; SET max_parallel_workers_per_gather = 0; }
step s3sel	{ SELECT count(*), sum(ba_id), sum(x) FROM bt; }
step s3row	{ SELECT id, ba_id, x FROM bt WHERE id IN (9, 12, 33, 18, 27, 36, 3) ORDER BY id; }
step s3pu	{ EXPLAIN (COSTS OFF) UPDATE bt SET x = x + 1 FROM ba WHERE bt.ba_id = ba.id AND ba.w = 1 AND bt.id < 60; }
step s3pl	{ EXPLAIN (COSTS OFF) UPDATE bt SET x = x + 1 FROM ba LEFT JOIN bb ON bb.ba_id = ba.id WHERE bt.ba_id = ba.id AND ba.w = 1 AND bt.id < 60; }
step s3pd	{ EXPLAIN (COSTS OFF) DELETE FROM bt USING ba WHERE bt.ba_id = ba.id AND ba.w = 1 AND bt.id < 60; }

permutation s3pu s3pl s3pd s1c s2c
permutation s1x s1k s1j s1d s1a s2u s1c s2c s3sel s3row
permutation s1x s1k s1j s1d s1b s1a s2l s1c s2c s3sel s3row
permutation s1x s1k s1j s1d s2d s1c s2c s3sel s3row
permutation s2u s1x s2c s1c s3sel s3row

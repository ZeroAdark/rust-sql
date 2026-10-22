# producerchain: an UPDATE / DELETE whose target's own scan is the outer of an INNER nested loop over another
# relation's plain heap scan (no Material: an index scan keyed by the outer row, or a seq scan under the join filter)
# meets a target row s1 changed. EvalPlanQual reruns the plan with the target's newest version and the other relation's
# row refetched at its junk ctid, the version the statement joined, its keys and the join filter checked again over
# the newest version: a version that still passes is written (SET over its new values), one whose key moved off the
# joined row (even onto a row another key would join) or that was deleted is skipped, and s1's change to the other
# relation is not seen.

setup
{
 CREATE TABLE pt (id int PRIMARY KEY, k int NOT NULL, x int NOT NULL);
 INSERT INTO pt SELECT i, i % 10, 0 FROM generate_series(1, 2000) i;
 CREATE TABLE ps (id int PRIMARY KEY, w int NOT NULL);
 INSERT INTO ps SELECT i, i % 5 FROM generate_series(1, 12) i;
 ANALYZE pt, ps;
}

teardown
{
 DROP TABLE pt;
 DROP TABLE ps;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1x	{ UPDATE pt SET x = x + 1000 WHERE id = 10; }
step s1k	{ UPDATE pt SET k = 7 WHERE id = 20; }
step s1d	{ DELETE FROM pt WHERE id = 30; }
step s1s	{ UPDATE ps SET w = w + 100 WHERE id = 1; }
step s1c	{ COMMIT; }

session s2
setup		{ SET enable_hashjoin = off; SET enable_mergejoin = off; SET enable_material = off; SET enable_memoize = off; SET max_parallel_workers_per_gather = 0; BEGIN ISOLATION LEVEL READ COMMITTED; }
step s2k	{ UPDATE pt SET x = x + ps.w FROM ps WHERE ps.id = pt.k + 1 AND pt.id <= 40 RETURNING pt.id, pt.k, pt.x, ps.id, ps.w; }
step s2f	{ UPDATE pt SET x = x + ps.id FROM ps WHERE pt.k = 0 AND ps.w = 0 AND ps.id < pt.id AND pt.id <= 40 RETURNING pt.id, pt.x; }
step s2d	{ DELETE FROM pt USING ps WHERE ps.id = pt.k + 1 AND ps.w IN (1, 2) AND pt.id <= 40 RETURNING pt.id, ps.id; }
step s2c	{ COMMIT; }

session s3
setup		{ SET enable_hashjoin = off; SET enable_mergejoin = off; SET enable_material = off; SET enable_memoize = off; SET max_parallel_workers_per_gather = 0; }
step s3sel	{ SELECT count(*), sum(k), sum(x) FROM pt; }
step s3row	{ SELECT id, k, x FROM pt WHERE id IN (10, 11, 20, 21, 30, 31, 40) ORDER BY id; }
step s3pk	{ EXPLAIN (COSTS OFF) UPDATE pt SET x = x + ps.w FROM ps WHERE ps.id = pt.k + 1 AND pt.id <= 40; }
step s3pf	{ EXPLAIN (COSTS OFF) UPDATE pt SET x = x + ps.id FROM ps WHERE pt.k = 0 AND ps.w = 0 AND ps.id < pt.id AND pt.id <= 40; }
step s3pd	{ EXPLAIN (COSTS OFF) DELETE FROM pt USING ps WHERE ps.id = pt.k + 1 AND ps.w IN (1, 2) AND pt.id <= 40; }

permutation s3pk s3pf s3pd s1c s2c
permutation s1x s1k s1d s1s s2k s1c s2c s3sel s3row
permutation s1x s1d s2f s1c s2c s3sel s3row
permutation s1k s1d s2d s1c s2c s3sel s3row
permutation s1x s1k s2f s1c s2c s3sel s3row
permutation s2k s1x s2c s1c s3sel s3row

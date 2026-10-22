# producerchain G1: an UPDATE / DELETE whose target's scan is the inner of the last of a chain of INNER nested loops,
# the earlier ones over other relations' plain heap scans (`UPDATE gt ... FROM ga, gb`: ga's scan the outer, gb's and
# then gt's scans the inners), meets a target row s1 changed. EvalPlanQual reruns the plan with the target's newest
# version and every other relation's row refetched at its junk ctid, the version the statement joined: a version that
# still joins is written (SET over its new values), one whose key moved off the joined row or that was deleted is
# skipped, and s1's changes to the other relations are not seen.

setup
{
 CREATE TABLE ga (id int PRIMARY KEY, w int NOT NULL);
 INSERT INTO ga SELECT i, i % 3 FROM generate_series(1, 6) i;
 CREATE TABLE gb (id int PRIMARY KEY, ga_id int NOT NULL, v int NOT NULL);
 INSERT INTO gb SELECT i, 1 + i % 6, i * 10 FROM generate_series(1, 12) i;
 CREATE TABLE gt (id int PRIMARY KEY, gb_id int NOT NULL, x int NOT NULL);
 INSERT INTO gt SELECT i, 1 + i % 400, 0 FROM generate_series(1, 2000) i;
 CREATE INDEX gt_gb ON gt (gb_id);
 ANALYZE ga, gb, gt;
}

teardown
{
 DROP TABLE gt;
 DROP TABLE gb;
 DROP TABLE ga;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1x	{ UPDATE gt SET x = x + 1000 WHERE id = 405; }
step s1k	{ UPDATE gt SET gb_id = 5 WHERE id = 805; }
step s1d	{ DELETE FROM gt WHERE id = 1205; }
step s1b	{ UPDATE gb SET v = v + 100 WHERE id = 6; }
step s1a	{ UPDATE ga SET w = 2 WHERE id = 1; }
step s1c	{ COMMIT; }

session s2
setup		{ SET enable_hashjoin = off; SET enable_mergejoin = off; SET enable_material = off; SET enable_memoize = off; SET max_parallel_workers_per_gather = 0; BEGIN ISOLATION LEVEL READ COMMITTED; }
step s2u	{ UPDATE gt SET x = x + gb.v FROM ga, gb WHERE gb.ga_id = ga.id AND gt.gb_id = gb.id AND ga.w = 1 RETURNING gt.id, gt.gb_id, gt.x, gb.v, ga.id; }
step s2d	{ DELETE FROM gt USING ga, gb WHERE gb.ga_id = ga.id AND gt.gb_id = gb.id AND ga.w = 1 RETURNING gt.id, gb.id, ga.id; }
step s2c	{ COMMIT; }

session s3
setup		{ SET enable_hashjoin = off; SET enable_mergejoin = off; SET enable_material = off; SET enable_memoize = off; SET max_parallel_workers_per_gather = 0; }
step s3sel	{ SELECT count(*), sum(gb_id), sum(x) FROM gt; }
step s3row	{ SELECT id, gb_id, x FROM gt WHERE id IN (5, 405, 805, 1205, 1605, 2) ORDER BY id; }
step s3pu	{ EXPLAIN (COSTS OFF) UPDATE gt SET x = x + gb.v FROM ga, gb WHERE gb.ga_id = ga.id AND gt.gb_id = gb.id AND ga.w = 1; }
step s3pd	{ EXPLAIN (COSTS OFF) DELETE FROM gt USING ga, gb WHERE gb.ga_id = ga.id AND gt.gb_id = gb.id AND ga.w = 1; }

permutation s3pu s3pd s1c s2c
permutation s1x s1k s1d s1b s1a s2u s1c s2c s3sel s3row
permutation s1x s1k s1d s2d s1c s2c s3sel s3row
permutation s2u s1x s2c s1c s3sel s3row

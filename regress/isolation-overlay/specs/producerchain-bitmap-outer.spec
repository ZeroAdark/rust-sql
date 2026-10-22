# producerchain G5: an UPDATE / DELETE over a nested loop whose outer is another relation's bitmap heap scan (`UPDATE ot ...
# FROM oa WHERE ot.a_id = oa.id AND oa.id IN (...)`: oa's bitmap heap scan over its primary key the outer, the target's
# bitmap heap scan keyed by it the inner), meets a target row s1 changed. EvalPlanQual reruns the plan with the target's
# newest version and oa's row refetched at its junk ctid (the version the statement joined, whatever s1 did to it): a
# version whose key moved off the joined oa row is skipped, one that still joins is written, a deleted one is skipped.

setup
{
 CREATE TABLE oa (id int PRIMARY KEY, w int NOT NULL, tag text NOT NULL);
 INSERT INTO oa SELECT i, i % 3, 'a' || i FROM generate_series(1, 900) i;
 CREATE TABLE ot (id int PRIMARY KEY, a_id int NOT NULL, x int NOT NULL);
 INSERT INTO ot SELECT i, 1 + i % 900, 0 FROM generate_series(1, 9000) i;
 CREATE INDEX ot_a ON ot (a_id);
 ANALYZE oa, ot;
}

teardown
{
 DROP TABLE ot;
 DROP TABLE oa;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1x	{ UPDATE ot SET x = x + 1000 WHERE id IN (900, 903); }
step s1k	{ UPDATE ot SET a_id = 2 WHERE id = 1800; }
step s1j	{ UPDATE ot SET a_id = 4 WHERE id = 2700; }
step s1d	{ DELETE FROM ot WHERE id = 3600; }
step s1a	{ UPDATE oa SET tag = 'a4new', w = 9 WHERE id = 4; }
step s1e	{ DELETE FROM oa WHERE id = 1; }
step s1c	{ COMMIT; }

session s2
setup		{ SET enable_hashjoin = off; SET enable_mergejoin = off; SET enable_material = off; SET enable_memoize = off; SET enable_indexscan = off; SET max_parallel_workers_per_gather = 0; BEGIN ISOLATION LEVEL READ COMMITTED; }
step s2u	{ UPDATE ot SET x = x + 1 FROM oa WHERE ot.a_id = oa.id AND oa.id IN (1, 4) RETURNING ot.id, ot.a_id, ot.x, oa.tag, oa.w; }
step s2d	{ DELETE FROM ot USING oa WHERE ot.a_id = oa.id AND oa.id IN (1, 4) RETURNING ot.id, oa.tag; }
step s2c	{ COMMIT; }

session s3
setup		{ SET enable_hashjoin = off; SET enable_mergejoin = off; SET enable_material = off; SET enable_memoize = off; SET enable_indexscan = off; SET max_parallel_workers_per_gather = 0; }
step s3sel	{ SELECT count(*), sum(a_id), sum(x) FROM ot; }
step s3row	{ SELECT id, a_id, x FROM ot WHERE id IN (900, 903, 1800, 2700, 3600, 3) ORDER BY id; }
step s3pu	{ EXPLAIN (COSTS OFF) UPDATE ot SET x = x + 1 FROM oa WHERE ot.a_id = oa.id AND oa.id IN (1, 4); }
step s3pd	{ EXPLAIN (COSTS OFF) DELETE FROM ot USING oa WHERE ot.a_id = oa.id AND oa.id IN (1, 4); }

permutation s3pu s3pd s1c s2c
permutation s1x s1k s1j s1d s1a s1e s2u s1c s2c s3sel s3row
permutation s1x s1k s1j s1d s1a s2d s1c s2c s3sel s3row
permutation s2u s1x s2c s1c s3sel s3row

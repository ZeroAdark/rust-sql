# D2734 ROWC-UNION (modifytable, gen mt-0354's form): a data-modifying CTE's DELETE through a partitioned root (the second
# leaf keeps its columns in another order) reads one settled union of the leaves' scans and meets rows s1 changed in
# both leaves. The recheck runs in the IR (D655 Row C): the row's own leaf's scan over its newest version, the other
# leaves blocked. A newest version that still passes the WHERE is deleted and returned as it is now, one that fails it
# stays, a deleted one is skipped, and one s1 moved to the other leaf raises 40001. With s1 rolled back, s2 deletes the
# versions it saw.

setup
{
 CREATE TABLE rp (id int NOT NULL, k int, v int, pad text) PARTITION BY RANGE (id);
 CREATE TABLE rp_1 PARTITION OF rp FOR VALUES FROM (1) TO (5);
 CREATE TABLE rp_2 (pad text, v int, k int, id int NOT NULL);
 ALTER TABLE rp ATTACH PARTITION rp_2 FOR VALUES FROM (5) TO (9);
 INSERT INTO rp SELECT i, i % 2, i * 10, 'p' || i FROM generate_series(1, 8) i;
 ANALYZE rp;
}

teardown
{
 DROP TABLE rp;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1v	{ UPDATE rp SET v = v + 1000 WHERE id IN (2, 6); }
step s1k	{ UPDATE rp SET k = 1 WHERE id IN (4, 8); }
step s1x	{ DELETE FROM rp WHERE id = 2; }
step s1m	{ UPDATE rp SET id = 7 WHERE id = 4; }
step s1c	{ COMMIT; }
step s1r	{ ROLLBACK; }

session s2
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s2d	{ WITH d AS (DELETE FROM rp WHERE k = 0 RETURNING id, v, pad) SELECT id, v, pad FROM d ORDER BY id; }
step s2n	{ WITH d AS (DELETE FROM rp WHERE k = 0) SELECT 1 AS one; }
step s2c	{ COMMIT; }

session s3
step s3sel	{ SELECT tableoid::regclass, id, k, v, pad FROM rp ORDER BY id; }

permutation s1v s1k s2d s1c s2c s3sel
permutation s1x s1k s2d s1c s2c s3sel
permutation s1v s1k s2n s1c s2c s3sel
permutation s1m s2d s1c s2c s3sel
permutation s1v s1x s2d s1r s2c s3sel

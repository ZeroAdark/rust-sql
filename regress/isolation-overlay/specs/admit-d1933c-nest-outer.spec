# admit D1933 (c): an UPDATE / DELETE whose target's own scan is the outer of a nested loop over a Material of another
# relation (a join PostgreSQL plans so: no hash or merge join takes `os.id < ot.k`) meets a target row s1 changed.
# EvalPlanQual reruns the plan with the target's newest version and the other relation's row refetched at its junk
# ctid, the version the statement joined, so s1's change to that relation is not seen: a version that still passes the
# join filter and its own filter is written (SET over the new values), one that no longer passes (its key moved) or
# was deleted is skipped; a DELETE's row joined twice is deleted once. A version whose key moved so that another row of
# the relation would join it is still skipped (the join filter runs over the refetched row alone). The DELETE's target
# scan is a bitmap heap scan: a version whose id s1 moved out of its index condition fails the recheck and is skipped.

setup
{
 CREATE TABLE ot (id int PRIMARY KEY, k int NOT NULL, x int NOT NULL);
 INSERT INTO ot SELECT i, i % 50, 0 FROM generate_series(1, 400) i;
 CREATE TABLE os (id int, w int);
 INSERT INTO os SELECT i, i % 9 FROM generate_series(1, 60) i;
 ANALYZE ot, os;
}

teardown
{
 DROP TABLE ot;
 DROP TABLE os;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1x	{ UPDATE ot SET x = x + 1000 WHERE id = 10; }
step s1k	{ UPDATE ot SET k = 1 WHERE id = 20; }
step s1d	{ DELETE FROM ot WHERE id = 30; }
step s1s	{ UPDATE os SET w = 0 WHERE id = 7; }
step s1i	{ UPDATE ot SET id = id + 1000 WHERE id = 12; }
step s1c	{ COMMIT; }

session s2
setup		{ SET enable_hashjoin = off; SET enable_mergejoin = off; SET max_parallel_workers_per_gather = 0; BEGIN ISOLATION LEVEL READ COMMITTED; }
step s2u	{ UPDATE ot SET x = x + os.id FROM os WHERE os.id < ot.k AND os.w = 7 AND ot.id <= 40 RETURNING ot.id, ot.k, ot.x; }
step s2d	{ DELETE FROM ot USING os WHERE os.id BETWEEN ot.k - 1 AND ot.k AND os.w IN (1, 2) AND ot.x = 0 AND ot.id <= 40 RETURNING ot.id, ot.k; }
step s2b	{ UPDATE ot SET x = x + os.id FROM os WHERE os.id BETWEEN ot.k - 1 AND ot.k AND os.w IN (1, 2) AND ot.id <= 40 RETURNING ot.id, ot.k, ot.x; }
step s2c	{ COMMIT; }

session s3
setup		{ SET enable_hashjoin = off; SET enable_mergejoin = off; SET max_parallel_workers_per_gather = 0; }
step s3sel	{ SELECT count(*), sum(k), sum(x) FROM ot; }
step s3row	{ SELECT id, k, x FROM ot WHERE id IN (10, 11, 12, 20, 30, 40, 51, 52, 1012) ORDER BY id; }
step s3pu	{ EXPLAIN (COSTS OFF) UPDATE ot SET x = x + os.id FROM os WHERE os.id < ot.k AND os.w = 7 AND ot.id <= 40; }
step s3pb	{ EXPLAIN (COSTS OFF) UPDATE ot SET x = x + os.id FROM os WHERE os.id BETWEEN ot.k - 1 AND ot.k AND os.w IN (1, 2) AND ot.id <= 40; }
step s3pd	{ EXPLAIN (COSTS OFF) DELETE FROM ot USING os WHERE os.id BETWEEN ot.k - 1 AND ot.k AND os.w IN (1, 2) AND ot.x = 0 AND ot.id <= 40; }

permutation s3pu s3pb s3pd s1c s2c
permutation s1x s1k s1d s2u s1c s2c s3sel s3row
permutation s1x s1k s1d s1s s2u s1c s2c s3sel s3row
permutation s1k s1d s1i s2d s1c s2c s3sel s3row
permutation s1x s1k s2b s1c s2c s3sel s3row
permutation s2u s1x s2c s1c s3sel s3row

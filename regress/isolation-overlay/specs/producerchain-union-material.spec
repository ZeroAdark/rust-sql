# producerchain rules#445: an UPDATE / DELETE whose target's own scan is the outer of a nested loop over a Material of an
# inheritance parent's members (FROM the parent: Materialize over the Append of up, uc1 and uc2, the target uc1 itself
# one of them) meets a target row s1 changed. EvalPlanQual reruns the plan with the target's newest version and each
# member's row refetched at the junk ctid and tableoid the statement joined (the other members return nothing), so s1's
# change to a member row is not seen: a version that still passes the join filter is written, one whose key moved off
# the joined row or was deleted is skipped, a DELETE's row joined twice is deleted once.

setup
{
 CREATE TABLE up (a int, k int NOT NULL, x int NOT NULL);
 CREATE TABLE uc1 () INHERITS (up);
 CREATE TABLE uc2 () INHERITS (up);
 INSERT INTO up SELECT i, i % 9, 0 FROM generate_series(1, 40) i;
 INSERT INTO uc1 SELECT i, i % 50, 0 FROM generate_series(1, 400) i;
 INSERT INTO uc2 SELECT i, i % 11, 0 FROM generate_series(1, 30) i;
 ANALYZE up, uc1, uc2;
}

teardown
{
 DROP TABLE up CASCADE;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1x	{ UPDATE uc1 SET x = x + 1000 WHERE a = 10; }
step s1k	{ UPDATE uc1 SET k = 1 WHERE a = 20; }
step s1d	{ DELETE FROM uc1 WHERE a = 30; }
step s1s	{ UPDATE ONLY up SET k = 0 WHERE a = 7; }
step s1c	{ COMMIT; }

session s2
setup		{ SET enable_hashjoin = off; SET enable_mergejoin = off; SET max_parallel_workers_per_gather = 0; BEGIN ISOLATION LEVEL READ COMMITTED; }
step s2u	{ UPDATE uc1 SET x = uc1.x + up.a FROM up WHERE up.a < uc1.k AND up.k = 7 AND uc1.a <= 40 RETURNING uc1.a, uc1.k, uc1.x; }
step s2d	{ DELETE FROM uc1 USING up WHERE up.a BETWEEN uc1.k - 1 AND uc1.k AND up.k IN (1, 2) AND uc1.x = 0 AND uc1.a <= 40 RETURNING uc1.a, uc1.k; }
step s2c	{ COMMIT; }

session s3
setup		{ SET enable_hashjoin = off; SET enable_mergejoin = off; SET max_parallel_workers_per_gather = 0; }
step s3sel	{ SELECT count(*), sum(k), sum(x) FROM ONLY uc1; }
step s3row	{ SELECT a, k, x FROM ONLY uc1 WHERE a IN (8, 9, 10, 11, 20, 30, 40) ORDER BY a; }
step s3pu	{ EXPLAIN (COSTS OFF) UPDATE uc1 SET x = uc1.x + up.a FROM up WHERE up.a < uc1.k AND up.k = 7 AND uc1.a <= 40; }
step s3pd	{ EXPLAIN (COSTS OFF) DELETE FROM uc1 USING up WHERE up.a BETWEEN uc1.k - 1 AND uc1.k AND up.k IN (1, 2) AND uc1.x = 0 AND uc1.a <= 40; }

permutation s3pu s3pd s1c s2c
permutation s1x s1k s1d s2u s1c s2c s3sel s3row
permutation s1x s1k s1d s1s s2u s1c s2c s3sel s3row
permutation s1k s1d s2d s1c s2c s3sel s3row
permutation s2u s1x s2c s1c s3sel s3row

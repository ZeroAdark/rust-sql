# D3379 (UNION-COPY, a subquery member): an UPDATE / DELETE of a partitioned target FROM a UNION ALL subquery whose
# second arm projects under a WHERE, so the planner keeps that arm a Subquery Scan over its relation's scan; a merge
# join of two Sorts over Appends (the union's arms, the result relations' arms). The subquery's COPY mark names a
# relation no scan carries, the kept arm's scan included: EvalPlanQual hands its stored row to no scan and scans every
# member again under the statement's snapshot (s1's insert into and delete from their table stay unseen), the row's
# own result relation at its newest version and the others empty. A version that still joins is written over its new
# values, a deleted one is skipped, a re-keyed one that joins no member row is skipped, and a row moved to another
# partition raises the serialization failure. Expected output is PostgreSQL 18.6's (the C oracle).

setup
{
 CREATE TABLE us (a int, n int);
 INSERT INTO us VALUES (0, 1), (1, 2), (2, 4), (1, 5);
 CREATE TABLE ut (a int, b text, x int) PARTITION BY LIST (a);
 CREATE TABLE ut1 PARTITION OF ut FOR VALUES IN (1);
 CREATE TABLE ut2 PARTITION OF ut FOR VALUES IN (2);
 CREATE TABLE ut3 PARTITION OF ut FOR VALUES IN (3);
 CREATE TABLE ut4 PARTITION OF ut FOR VALUES IN (4);
 INSERT INTO ut VALUES (1, 'a', 0), (2, 'a', 0), (3, 'a', 0), (4, 'a', 0);
}

teardown
{
 DROP TABLE ut;
 DROP TABLE us;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1x	{ UPDATE ut SET x = x + 10 WHERE a IN (1, 2, 3); }
step s1d	{ DELETE FROM ut WHERE a = 3; }
step s1i	{ INSERT INTO us VALUES (3, 6), (0, 8); }
step s1r	{ DELETE FROM us WHERE n IN (2, 4); }
step s1m	{ UPDATE ut SET a = 4 WHERE a = 2; }
step s1c	{ COMMIT; }

session s2
setup		{ SET enable_hashjoin = off; SET enable_nestloop = off; SET max_parallel_workers_per_gather = 0; BEGIN ISOLATION LEVEL READ COMMITTED; }
step s2u	{ UPDATE ut SET b = 'u', x = ut.x + 1 FROM (SELECT a FROM us UNION ALL SELECT a + 1 FROM us WHERE n % 2 = 0) ss (a) WHERE ut.a = ss.a; }
step s2d	{ DELETE FROM ut USING (SELECT a FROM us UNION ALL SELECT a + 1 FROM us WHERE n % 2 = 0) ss (a) WHERE ut.a = ss.a; }
step s2c	{ COMMIT; }

session s3
setup		{ SET enable_hashjoin = off; SET enable_nestloop = off; SET max_parallel_workers_per_gather = 0; }
step s3sel	{ SELECT tableoid::regclass::text AS rel, a, b, x FROM ut ORDER BY a, rel; }
step s3pu	{ EXPLAIN (COSTS OFF) UPDATE ut SET b = 'u', x = ut.x + 1 FROM (SELECT a FROM us UNION ALL SELECT a + 1 FROM us WHERE n % 2 = 0) ss (a) WHERE ut.a = ss.a; }

permutation s3pu s1c s2c
permutation s1x s1d s1i s1r s2u s1c s2c s3sel
permutation s2u s1x s2c s1c s3sel
permutation s1m s2u s1c s2c s3sel
permutation s1x s1d s1i s1r s2d s1c s2c s3sel
permutation s1m s2d s1c s2c s3sel

# producerchain UNION-NEST (inherit#145): an UPDATE of a partitioned target FROM a UNION ALL subquery the planner
# flattens into an appendrel, under an OR join qual no hash or merge join takes: a nested loop whose outer is the Append
# of the union's arms and whose inner is the Materialize over the Append of the result relations' scans (the regress's
# shape over tables never analyzed). The subquery's COPY mark names a relation no scan carries, so EvalPlanQual scans the
# union's members again, whole, under the statement's snapshot (s1's insert into and delete from their tables stay
# unseen), with the row's own result relation at its newest version and the others empty, and takes the first row the
# loop yields. A version that still joins is written over its new values (s2v's SET reads that first partner's value), one
# that no longer joins or was deleted is skipped, and a row moved to another partition raises the serialization failure.

setup
{
 CREATE TABLE us (a int);
 INSERT INTO us VALUES (0), (1);
 CREATE TABLE us_child () INHERITS (us);
 INSERT INTO us_child VALUES (1), (2);
 CREATE TABLE ut (a int, b char, c text, x int) PARTITION BY LIST (a);
 CREATE TABLE ut1 PARTITION OF ut FOR VALUES IN (1);
 CREATE TABLE ut2 PARTITION OF ut FOR VALUES IN (2) PARTITION BY LIST (b);
 CREATE TABLE ut3 PARTITION OF ut FOR VALUES IN (3);
 CREATE TABLE ut2a PARTITION OF ut2 FOR VALUES IN ('a');
 CREATE TABLE ut2b PARTITION OF ut2 FOR VALUES IN ('b');
 INSERT INTO ut VALUES (1, 'b', 'p', 0), (2, 'a', 'p', 0), (2, 'b', 'p', 0), (3, 'a', 'p', 0);
}

teardown
{
 DROP TABLE ut;
 DROP TABLE us CASCADE;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1x	{ UPDATE ut SET x = x + 10 WHERE a IN (1, 3); }
step s1k	{ UPDATE ut SET c = 'k' WHERE a = 1; }
step s1d	{ DELETE FROM ut WHERE a = 3; }
step s1i	{ INSERT INTO us VALUES (7), (8); }
step s1r	{ DELETE FROM us WHERE a = 0; }
step s1m	{ UPDATE ut SET b = 'a' WHERE a = 2 AND b = 'b'; }
step s1c	{ COMMIT; }

session s2
setup		{ SET enable_hashjoin = off; SET enable_mergejoin = off; SET max_parallel_workers_per_gather = 0; BEGIN ISOLATION LEVEL READ COMMITTED; }
step s2u	{ UPDATE ut SET c = 'u', x = ut.x + 1 FROM (SELECT a FROM us UNION ALL SELECT a + 1 FROM us) ss (a) WHERE (ut.a = ss.a AND ut.b = 'b') OR ut.a = 3; }
step s2v	{ UPDATE ut SET c = 'v', x = ut.x + 100 * ss.a FROM (SELECT a FROM us UNION ALL SELECT a + 1 FROM us) ss (a) WHERE (ut.a = ss.a AND ut.b = 'b') OR ut.a = 3; }
step s2w	{ UPDATE ut SET c = 'w' FROM (SELECT a FROM us UNION ALL SELECT a + 1 FROM us) ss (a) WHERE (ut.a = ss.a AND ut.c = 'p') OR ut.a = 3; }
step s2c	{ COMMIT; }

session s3
setup		{ SET enable_hashjoin = off; SET enable_mergejoin = off; SET max_parallel_workers_per_gather = 0; }
step s3sel	{ SELECT tableoid::regclass::text AS rel, a, b, c, x FROM ut ORDER BY a, b, rel; }
step s3pu	{ EXPLAIN (COSTS OFF) UPDATE ut SET c = 'u', x = ut.x + 1 FROM (SELECT a FROM us UNION ALL SELECT a + 1 FROM us) ss (a) WHERE (ut.a = ss.a AND ut.b = 'b') OR ut.a = 3; }

permutation s3pu s1c s2c
permutation s1x s1d s1i s1r s2u s1c s2c s3sel
permutation s2u s1x s2c s1c s3sel
permutation s1x s2v s1c s2c s3sel
permutation s1k s2w s1c s2c s3sel
permutation s1m s2u s1c s2c s3sel
permutation s1i s1r s1k s2v s1c s2c s3sel

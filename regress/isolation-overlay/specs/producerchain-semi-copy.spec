# producerchain pj211 (partition_join#211): an UPDATE / DELETE of a partitioned target (or a plain one) whose WHERE EXISTS
# is a Nested Loop Semi Join over the targets' Append, its inner a Materialize over a Nested Loop of a plain relation
# (REFERENCE-marked) and a LATERAL subquery with a LIMIT (Subquery Scan, COPY-marked), meets target rows s1 changed.
# EvalPlanQual reruns the plan with the target's newest version, the plain relation's row refetched at its junk ctid
# (the version the join read) and the subquery's stored row: a version that still passes the target's own filter is
# written, one whose key moved off the filter or was deleted is skipped, and s1's change to the inner relation is not
# seen.

setup
{
 CREATE TABLE si4 (f1 int4);
 INSERT INTO si4 VALUES (0), (123456), (-123456);
 CREATE TABLE si8 (q1 int8, q2 int8);
 INSERT INTO si8 VALUES (123, 456), (123, 789), (456, 123);
 CREATE TABLE sp (a int, k int, x int) PARTITION BY RANGE (a);
 CREATE TABLE sp1 PARTITION OF sp FOR VALUES FROM (0) TO (100);
 CREATE TABLE sp2 PARTITION OF sp FOR VALUES FROM (100) TO (200);
 INSERT INTO sp SELECT i, i % 7, 0 FROM generate_series(0, 199, 3) i;
 CREATE TABLE st (a int, k int, x int);
 INSERT INTO st SELECT i, i % 5, 0 FROM generate_series(1, 40) i;
 ANALYZE si4, si8, sp, st;
}

teardown
{
 DROP TABLE sp, st, si4, si8;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1k	{ UPDATE sp SET k = 4 WHERE a = 24; }
step s1x	{ UPDATE sp SET x = x + 100 WHERE a = 108; }
step s1d	{ DELETE FROM sp WHERE a = 150; }
step s1i	{ DELETE FROM si4 WHERE f1 = 0; }
step s1t	{ UPDATE st SET k = 0 WHERE a = 7; }
step s1u	{ UPDATE st SET x = x + 9 WHERE a = 12; }
step s1c	{ COMMIT; }

session s2
setup		{ SET enable_hashjoin = off; SET enable_mergejoin = off; SET max_parallel_workers_per_gather = 0; BEGIN ISOLATION LEVEL READ COMMITTED; }
step s2u	{ UPDATE sp SET x = x + 1 WHERE EXISTS (SELECT 1 FROM si4, LATERAL (SELECT si4.f1 FROM si8 LIMIT 2) ss WHERE sp.k = 3) RETURNING a, k, x; }
step s2d	{ DELETE FROM sp WHERE EXISTS (SELECT 1 FROM si4, LATERAL (SELECT si4.f1 FROM si8 LIMIT 2) ss WHERE sp.k = 3) RETURNING a, k; }
step s2t	{ DELETE FROM st WHERE EXISTS (SELECT 1 FROM si4, LATERAL (SELECT si4.f1 FROM si8 LIMIT 2) ss WHERE st.k = 2) RETURNING a, k, x; }
step s2c	{ COMMIT; }

session s3
setup		{ SET enable_hashjoin = off; SET enable_mergejoin = off; SET max_parallel_workers_per_gather = 0; }
step s3sel	{ SELECT tableoid::regclass, count(*), sum(k), sum(x) FROM sp GROUP BY 1 ORDER BY 1; }
step s3row	{ SELECT tableoid::regclass, a, k, x FROM sp WHERE a IN (24, 108, 150, 3, 171) ORDER BY 1, 2; }
step s3st	{ SELECT a, k, x FROM st WHERE a IN (2, 7, 12, 17) ORDER BY 1; }
step s3pu	{ EXPLAIN (COSTS OFF) UPDATE sp SET x = x + 1 WHERE EXISTS (SELECT 1 FROM si4, LATERAL (SELECT si4.f1 FROM si8 LIMIT 2) ss WHERE sp.k = 3); }
step s3pt	{ EXPLAIN (COSTS OFF) DELETE FROM st WHERE EXISTS (SELECT 1 FROM si4, LATERAL (SELECT si4.f1 FROM si8 LIMIT 2) ss WHERE st.k = 2); }

permutation s3pu s3pt s1c s2c
permutation s1k s1x s1d s1i s2u s1c s2c s3sel s3row
permutation s1k s1d s2d s1c s2c s3sel s3row
permutation s2u s1k s2c s1c s3sel s3row
permutation s1t s1u s1i s2t s1c s2c s3st

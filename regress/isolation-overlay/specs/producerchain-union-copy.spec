# producerchain UNION-COPY (inherit#130 #134): an UPDATE of a partitioned target FROM a UNION ALL subquery the planner
# flattens into an appendrel, joined by a merge join of two Sorts over Appends (the union's arms, the result relations'
# arms; the targets the merge's inner in s2s, the regress's own shape, its outer in s2u and s2v). The subquery's COPY mark names a relation no scan
# carries: EvalPlanQual hands its stored row to no scan and scans the union's members again under the statement's
# snapshot (s1's insert into and delete from their tables stay unseen), the row's own result relation at its newest
# version and the others empty. A version that still joins is written over its new values, a deleted one is skipped,
# and a row moved to another partition raises the serialization failure. s2o: the targets' sorted arms the merge's outer
# beside an inheritance parent's REFERENCE-marked arms (updatable_views#648 #649's shape): each joined child row refetched.

setup
{
 CREATE TABLE us (a int);
 INSERT INTO us VALUES (0), (1);
 CREATE TABLE us_child () INHERITS (us);
 INSERT INTO us_child VALUES (1), (2);
 CREATE TABLE ut (a int, b text, x int) PARTITION BY LIST (a);
 CREATE TABLE ut1 PARTITION OF ut FOR VALUES IN (1);
 CREATE TABLE ut2 PARTITION OF ut FOR VALUES IN (2);
 CREATE TABLE ut3 PARTITION OF ut FOR VALUES IN (3);
 INSERT INTO ut VALUES (1, 'a', 0), (2, 'a', 0), (3, 'a', 0);
 CREATE TABLE pt (a int, b char) PARTITION BY LIST (a);
 CREATE TABLE pt1 PARTITION OF pt FOR VALUES IN (1);
 CREATE TABLE pt2 PARTITION OF pt FOR VALUES IN (2);
 CREATE TABLE pt3 PARTITION OF pt FOR VALUES IN (3);
 INSERT INTO pt VALUES (1, 'a'), (2, 'a'), (3, 'a');
 CREATE TABLE op (id int, w int);
 CREATE TABLE op_child () INHERITS (op);
 INSERT INTO op VALUES (1, 10), (3, 30);
 INSERT INTO op_child VALUES (2, 20), (5, 50);
}

teardown
{
 DROP TABLE ut;
 DROP TABLE pt;
 DROP TABLE op CASCADE;
 DROP TABLE us CASCADE;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1x	{ UPDATE ut SET x = x + 10 WHERE a IN (1, 2); }
step s1d	{ DELETE FROM ut WHERE a = 3; }
step s1i	{ INSERT INTO us VALUES (3), (5); }
step s1r	{ DELETE FROM us_child WHERE a = 2; }
step s1m	{ UPDATE ut SET a = 3 WHERE a = 2; }
step s1y	{ UPDATE pt SET b = 'y' WHERE a = 1; }
step s1e	{ DELETE FROM pt WHERE a = 2; }
step s1o	{ UPDATE op SET w = w + 100 WHERE id = 2; }
step s1q	{ DELETE FROM op WHERE id = 1; }
step s1c	{ COMMIT; }

session s2
setup		{ SET enable_hashjoin = off; SET enable_nestloop = off; SET max_parallel_workers_per_gather = 0; BEGIN ISOLATION LEVEL READ COMMITTED; }
step s2u	{ UPDATE ut SET b = 'u', x = ut.x + 1 FROM (SELECT a FROM us UNION ALL SELECT a FROM us) ss (a) WHERE ut.a = ss.a; }
step s2v	{ UPDATE ut SET b = 'v', x = ut.x + 2 FROM (SELECT 0 FROM ut UNION ALL SELECT 1 FROM ut) ss (a) WHERE ut.a = ss.a; }
step s2s	{ UPDATE pt SET b = 'b' FROM (SELECT a FROM us UNION ALL SELECT a + 1 FROM us) ss (a) WHERE pt.a = ss.a; }
step s2o	{ UPDATE ut SET b = 'o', x = ut.x + op.w FROM op WHERE ut.a = op.id; }
step s2c	{ COMMIT; }

session s3
setup		{ SET enable_hashjoin = off; SET enable_nestloop = off; SET max_parallel_workers_per_gather = 0; }
step s3sel	{ SELECT tableoid::regclass::text AS rel, a, b, x FROM ut ORDER BY a, rel; }
step s3pt	{ SELECT tableoid::regclass::text AS rel, a, b FROM pt ORDER BY a, rel; }
step s3po	{ EXPLAIN (COSTS OFF) UPDATE ut SET b = 'o', x = ut.x + op.w FROM op WHERE ut.a = op.id; }
step s3ps	{ EXPLAIN (COSTS OFF) UPDATE pt SET b = 'b' FROM (SELECT a FROM us UNION ALL SELECT a + 1 FROM us) ss (a) WHERE pt.a = ss.a; }
step s3pu	{ EXPLAIN (COSTS OFF) UPDATE ut SET b = 'u', x = ut.x + 1 FROM (SELECT a FROM us UNION ALL SELECT a FROM us) ss (a) WHERE ut.a = ss.a; }
step s3pv	{ EXPLAIN (COSTS OFF) UPDATE ut SET b = 'v', x = ut.x + 2 FROM (SELECT 0 FROM ut UNION ALL SELECT 1 FROM ut) ss (a) WHERE ut.a = ss.a; }

permutation s3pu s3pv s3ps s3po s1c s2c
permutation s1x s1d s1i s1r s2u s1c s2c s3sel
permutation s1x s1d s1i s1r s2v s1c s2c s3sel
permutation s2u s1x s2c s1c s3sel
permutation s1m s2u s1c s2c s3sel
permutation s1y s1e s1i s1r s2s s1c s2c s3pt
permutation s2s s1y s2c s1c s3pt
permutation s1x s1o s1q s2o s1c s2c s3sel
permutation s2o s1o s2c s1c s3sel

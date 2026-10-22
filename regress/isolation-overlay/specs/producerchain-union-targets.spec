# producerchain UNION: an UPDATE of several result relations whose rows come out of one settled union of their scans
# (decompose cannot stream the write into the Append's arms: a MULTIEXPR SubPlan in the SET list, over an inheritance
# tree whose child keeps its columns in another order or over a partitioned table, the row maybe moving to another
# partition) meets a target row s1 changed. PostgreSQL takes each
# row's result relation from its junk tableoid and its position from its junk ctid; EvalPlanQual substitutes that
# relation's scan with the newest version and blocks every other result relation's scan (relsubs_blocked), so only that
# version is rechecked: one that still qualifies is written over its new values (the SubPlan over it), one that no longer qualifies or was deleted is skipped, the other relations' rows are neither seen again nor
# written twice, and a row s1 moved to another partition raises 40001.

setup
{
 CREATE TABLE ut_one (k int);
 INSERT INTO ut_one VALUES (7);
 CREATE TABLE ut_par (f1 int, f2 text);
 CREATE TABLE ut_cld (z int, f1 int, f2 text);
 ALTER TABLE ut_cld INHERIT ut_par;
 INSERT INTO ut_par VALUES (1, 'p1'), (2, 'p2'), (6, 'p6');
 INSERT INTO ut_cld VALUES (101, 1, 'c1'), (103, 3, 'c3'), (104, 4, 'c4'), (109, 9, 'c9');
 CREATE TABLE ut_rp (a text, b bigint, c numeric, d int) PARTITION BY RANGE (a, b);
 CREATE TABLE ut_rp_a1 PARTITION OF ut_rp FOR VALUES FROM ('a', 1) TO ('a', 10);
 CREATE TABLE ut_rp_a2 (d int, c numeric, b bigint, a text);
 ALTER TABLE ut_rp ATTACH PARTITION ut_rp_a2 FOR VALUES FROM ('a', 10) TO ('a', 20);
 CREATE TABLE ut_rp_b1 PARTITION OF ut_rp FOR VALUES FROM ('b', 1) TO ('b', 20);
 INSERT INTO ut_rp VALUES ('a', 1, 200, 1), ('a', 5, 150, 2), ('a', 12, 200, 3), ('a', 15, 90, 4), ('b', 3, 200, 5);
}

teardown
{
 DROP TABLE ut_rp;
 DROP TABLE ut_cld;
 DROP TABLE ut_par;
 DROP TABLE ut_one;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1f	{ UPDATE ut_cld SET f2 = 'C3' WHERE f1 = 3; }
step s1k	{ UPDATE ut_cld SET f1 = 8 WHERE f1 = 4; }
step s1d	{ DELETE FROM ut_par WHERE f1 = 2; }
step s1r	{ UPDATE ut_rp SET d = d + 10 WHERE b = 12; }
step s1x	{ UPDATE ut_rp SET c = 201 WHERE b = 1; }
step s1y	{ UPDATE ut_rp SET c = 160 WHERE b = 12; }
step s1m	{ UPDATE ut_rp SET b = 13 WHERE b = 5; }
step s1c	{ COMMIT; }

session s2
setup		{ SET max_parallel_workers_per_gather = 0; BEGIN ISOLATION LEVEL READ COMMITTED; }
step s2i	{ UPDATE ut_par i SET (f1, f2) = (SELECT i.f1 + k, i.f2 || k FROM ut_one LIMIT 1) WHERE i.f1 < 5; }
step s2r	{ UPDATE ut_rp r SET (c) = (SELECT r.c - 100 FROM ut_one LIMIT 1) WHERE a = 'a' AND c >= 150; }
step s2m	{ UPDATE ut_rp r SET (a, c) = (SELECT 'b', r.c - 90 FROM ut_one LIMIT 1) WHERE a = 'a' AND c = 200; }
step s2c	{ COMMIT; }

session s3
setup		{ SET max_parallel_workers_per_gather = 0; }
step s3p	{ SELECT tableoid::regclass, f1, f2 FROM ut_par ORDER BY 1, 2, 3; }
step s3r	{ SELECT tableoid::regclass, a, b, c, d FROM ut_rp ORDER BY a, b; }
step s3e	{ EXPLAIN (COSTS OFF) UPDATE ut_par i SET (f1, f2) = (SELECT i.f1 + k, i.f2 || k FROM ut_one LIMIT 1) WHERE i.f1 < 5; }

permutation s3e s1f s1k s1d s2i s1c s2c s3p
permutation s1r s2r s1c s2c s3r
permutation s1x s1y s2r s1c s2c s3r
permutation s1x s2m s1c s2c s3r
permutation s1r s2m s1c s2c s3r
permutation s1m s2r s1c s2c s3r
permutation s2i s1f s2c s1c s3p

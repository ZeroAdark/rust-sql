# producerchain UNION (nest form): an UPDATE of several result relations joined to another relation by a nested loop
# whose outer is the other relation's scan and whose inner is the Append of the result relations' scans (under a
# Materialize, update 294's sub-partitioned target, or not, rowsecurity 336's inheritance tree; decompose settles that
# Append into a union the nest's inner reads) meets a target row s1 changed. EvalPlanQual substitutes the row's result
# relation's scan with the newest version and blocks every other result relation's, refetches the other relation's row
# at its junk ctid (the version the statement joined, s1's change to it unseen) and reruns the join: a version that still
# joins is written over its new values, one that was deleted is skipped, a row s1 moved to another partition raises
# 40001, and the other relations' rows are not written twice. um_lp / um_nq stay unanalyzed (under autovacuum's
# threshold), so PostgreSQL keeps the Materialize over the Append there: a recheck that refills the spool.

setup
{
 CREATE TABLE un_lp (a numeric, b int, c int8) PARTITION BY LIST (a);
 CREATE TABLE un_sp PARTITION OF un_lp FOR VALUES IN (1) PARTITION BY LIST (b);
 CREATE TABLE un_sp1 (b int, c int8, a numeric);
 ALTER TABLE un_sp ATTACH PARTITION un_sp1 FOR VALUES IN (1);
 CREATE TABLE un_sp2 (b int, c int8, a numeric);
 ALTER TABLE un_sp ATTACH PARTITION un_sp2 FOR VALUES IN (2);
 CREATE TABLE un_lp1 (a numeric, b int, c int8);
 ALTER TABLE un_lp ATTACH PARTITION un_lp1 FOR VALUES IN (2, 3);
 INSERT INTO un_lp VALUES (2, 5, 50), (3, 6, 60), (1, 1, 60), (1, 2, 10), (1, 1, 11), (1, 2, 12);
 INSERT INTO un_lp SELECT 1, 1 + i % 2, 1000 + i FROM generate_series(1, 400) i;
 CREATE TABLE un_np (id int, v int);
 INSERT INTO un_np VALUES (1, 100), (2, 200), (3, 300);
 CREATE TABLE un_nq (id int, v int);
 INSERT INTO un_nq VALUES (1, 100), (1, 100), (2, 200), (3, 300);
 CREATE TABLE ur1 (id int NOT NULL PRIMARY KEY, a int, junk1 text, b text);
 ALTER TABLE ur1 DROP COLUMN junk1;
 CREATE TABLE ur2 (c float) INHERITS (ur1);
 CREATE TABLE ur3 (id int NOT NULL PRIMARY KEY, c text, b text, a int);
 ALTER TABLE ur3 INHERIT ur1;
 INSERT INTO ur1 VALUES (1, 3, 'aaa'), (2, 4, 'bbb'), (3, 3, 'ccc');
 INSERT INTO ur2 VALUES (11, 3, 'abc', 1.1), (12, 4, 'bcd', 2.2);
 INSERT INTO ur3 VALUES (21, 'xxx', 'yyy', 3), (22, 'yyy', 'zzz', 4);
 INSERT INTO ur1 SELECT i, 100 + i, 'f' FROM generate_series(100, 300) i;
 INSERT INTO ur2 SELECT i, 100 + i, 'g', 0 FROM generate_series(400, 600) i;
 CREATE TABLE um_lp (a numeric, b int, c int8) PARTITION BY LIST (a);
 CREATE TABLE um_sp PARTITION OF um_lp FOR VALUES IN (1) PARTITION BY LIST (b);
 CREATE TABLE um_sp1 (b int, c int8, a numeric);
 ALTER TABLE um_sp ATTACH PARTITION um_sp1 FOR VALUES IN (1);
 CREATE TABLE um_sp2 (b int, c int8, a numeric);
 ALTER TABLE um_sp ATTACH PARTITION um_sp2 FOR VALUES IN (2);
 CREATE TABLE um_lp1 (a numeric, b int, c int8);
 ALTER TABLE um_lp ATTACH PARTITION um_lp1 FOR VALUES IN (2, 3);
 INSERT INTO um_lp VALUES (2, 5, 50), (3, 6, 60), (1, 1, 60), (1, 2, 10), (1, 1, 11), (1, 2, 12);
 CREATE TABLE um_nq (id int);
 INSERT INTO um_nq VALUES (1), (1), (1), (2), (2), (2), (3), (3), (3);
}
setup { ANALYZE un_lp, un_np, un_nq, ur1, ur2, ur3; }

teardown
{
 DROP TABLE un_lp;
 DROP TABLE un_np;
 DROP TABLE un_nq;
 DROP TABLE ur1 CASCADE;
 DROP TABLE um_lp;
 DROP TABLE um_nq;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1c	{ UPDATE un_sp1 SET c = c + 1 WHERE c = 60; }
step s1d	{ DELETE FROM un_sp1 WHERE c = 11; }
step s1n	{ UPDATE un_np SET v = v + 1 WHERE id = 1; }
step s1k	{ UPDATE un_lp SET a = 3 WHERE c = 12; }
step s1r	{ UPDATE ur3 SET b = 'Y' WHERE id = 21; }
step s1q	{ DELETE FROM ur1 WHERE id = 3; }
step s1m	{ UPDATE um_sp1 SET c = c + 1 WHERE c = 60; }
step s1e	{ DELETE FROM um_sp2 WHERE c = 10; }
step s1x	{ COMMIT; }

session s2
setup		{ SET max_parallel_workers_per_gather = 0; BEGIN ISOLATION LEVEL READ COMMITTED; }
step s2u	{ UPDATE un_lp t1 SET c = t1.c + t2.v FROM un_np t2 WHERE t1.a = t2.id AND a = 1; }
step s2m	{ UPDATE un_lp t1 SET a = 2 FROM un_np t2 WHERE t1.a = t2.id AND a = 1; }
step s2q	{ UPDATE un_lp t1 SET c = t1.c + t2.v FROM un_nq t2 WHERE t1.a = t2.id AND a = 1; }
step s2r	{ UPDATE ur1 SET b = ur1.b || '+' || ur2.id FROM ur2 WHERE ur1.a = 3 AND ur2.a = 3; }
step s2v	{ UPDATE um_lp t1 SET c = t1.c + 1 FROM um_nq t2 WHERE t1.a = t2.id AND a = 1; }
step s2c	{ COMMIT; }

session s3
setup		{ SET max_parallel_workers_per_gather = 0; }
step s3l	{ SELECT tableoid::regclass, a, b, c FROM un_lp WHERE c < 1000 OR c > 1500 ORDER BY 1, a, b, c; }
step s3s	{ SELECT tableoid::regclass, count(*), sum(c) FROM un_lp WHERE c BETWEEN 1000 AND 1500 GROUP BY 1 ORDER BY 1; }
step s3r	{ SELECT tableoid::regclass, id, a, b FROM ur1 ORDER BY 1, 2; }
step s3eu	{ EXPLAIN (COSTS OFF) UPDATE un_lp t1 SET c = t1.c + t2.v FROM un_np t2 WHERE t1.a = t2.id AND a = 1; }
step s3eq	{ EXPLAIN (COSTS OFF) UPDATE un_lp t1 SET c = t1.c + t2.v FROM un_nq t2 WHERE t1.a = t2.id AND a = 1; }
step s3m	{ SELECT tableoid::regclass, a, b, c FROM um_lp ORDER BY 1, a, b, c; }
step s3em	{ EXPLAIN (COSTS OFF) UPDATE um_lp t1 SET c = t1.c + 1 FROM um_nq t2 WHERE t1.a = t2.id AND a = 1; }
step s3er	{ EXPLAIN (COSTS OFF) UPDATE ur1 SET b = ur1.b || '+' || ur2.id FROM ur2 WHERE ur1.a = 3 AND ur2.a = 3; }

permutation s3eu s1c s1d s2u s1x s2c s3l s3s
permutation s1c s1n s2u s1x s2c s3l
permutation s1k s2u s1x s2c s3l
permutation s1c s2m s1x s2c s3l s3s
permutation s3eq s1c s1d s2q s1x s2c s3l s3s
permutation s3er s1r s1q s2r s1x s2c s3r
permutation s2u s1c s2c s1x s3l
permutation s3em s1m s1e s2v s1x s2c s3m

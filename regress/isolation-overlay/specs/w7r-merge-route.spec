# M8-WRITE V4R-498: a MERGE into a partitioned root whose only writes are NOT MATCHED INSERTs, routed per row, under a
# concurrent change (PostgreSQL 18: the join reads the statement's snapshot; the routed INSERT meets the leaf's unique
# index). The source (64 rows) is the join's outer, the root's one (DEFAULT) leaf on the hash side. After s1 commits or aborts:
#  (a) s1 inserted a key s2 would insert (uncommitted): s2's INSERT waits on it, then raises the unique violation
#  (b) the same, s1 aborts: s2 inserts the key
#  (c) s1 deleted a target row s2 joins (uncommitted, then committed): s2's snapshot still matches it (no action), no wait
# Expected output is PostgreSQL 18.6's (the C oracle).

setup
{
 CREATE TABLE rt (k int PRIMARY KEY, v int) PARTITION BY LIST (k);
 CREATE TABLE rtd PARTITION OF rt DEFAULT;
 CREATE TABLE rs (k int, w int);
 INSERT INTO rt VALUES (1, 10), (150, 20);
 INSERT INTO rs VALUES (1, 100), (5, 500), (150, 1500), (160, 1600);
 INSERT INTO rs SELECT g, 0 FROM generate_series(1000, 1059) g;
 ANALYZE rt, rs;
}

teardown
{
 DROP TABLE rt, rs;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1ins	{ INSERT INTO rt VALUES (160, -1); }
step s1del	{ DELETE FROM rt WHERE k = 150; }
step s1c	{ COMMIT; }
step s1a	{ ROLLBACK; }

session s2
setup
{
 SET enable_mergejoin = off;
 SET enable_nestloop = off;
}
step s2cnt	{ SELECT count(*) FROM rt; SELECT count(*) FROM rs; }
step s2plan	{ EXPLAIN (costs off) MERGE INTO rt t USING rs s ON t.k = s.k WHEN NOT MATCHED THEN INSERT VALUES (s.k, s.w); }
step s2merge	{ MERGE INTO rt t USING rs s ON t.k = s.k WHEN NOT MATCHED THEN INSERT VALUES (s.k, s.w); }
step s2chk	{ SELECT tableoid::regclass, * FROM rt WHERE k < 1000 ORDER BY k; SELECT count(*) FROM rt WHERE k >= 1000; }

permutation s2cnt s2plan
permutation s1ins s2merge s1c s2chk
permutation s1ins s2merge s1a s2chk
permutation s1del s2merge s1c s2chk

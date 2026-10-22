# M8-WRITE W4 (D1249): EvalPlanQual under a write whose rows come through an INNER merge join. The victim (s2 / s3)
# blocks on a target row s1 changed; after s1 commits, the recheck reruns both sorted inputs at one row each (the target's
# newest version, the source row at the version the join read, PostgreSQL's EvalPlanQualFetchRowMark) and the merge
# step over them. Arms:
#  (a) the target row's v changed: the newest version is written
#  (b) the target row's join key changed: the recheck no longer joins, the row is skipped
#  (c) the joined source row changed too: the recheck joins the version the scan read (REFERENCE), not the newest
#  (d) MERGE WHEN MATCHED under (a)
#  (e) DELETE .. USING under a concurrent DELETE: the row is skipped
#  (f) duplicate target keys: the recheck fires on the second row of a key group (mid-group, after a mark / restore);
#      the main pass continues past it with its merge state intact
#  (g) an index-ordered outer (no Sort on the target), under (a)
#  (h) a target Sort that spills (20000 rows, work_mem 64kB), one row rechecked
# Expected output is PostgreSQL 18.6's (the C oracle).

setup
{
 CREATE TABLE mt (id int, k int, v int);
 CREATE TABLE ms (k int, w int);
 INSERT INTO mt VALUES (1, 1, 10), (2, 2, 20), (3, 3, 30), (4, 4, 40), (5, 4, 50), (6, 4, 60), (7, 7, 70);
 INSERT INTO ms VALUES (1, 1), (2, 2), (4, 4), (7, 7);
 CREATE TABLE mbig (id int, k int, v int);
 CREATE TABLE mbs (k int, w int);
 INSERT INTO mbig SELECT g, g % 5000, 0 FROM generate_series(1, 20000) g;
 INSERT INTO mbs SELECT g, g FROM generate_series(0, 4999) g;
 CREATE INDEX mt_k ON mt (k);
 CREATE INDEX ms_k ON ms (k);
 ANALYZE mt, ms, mbig, mbs;
}

teardown
{
 DROP TABLE mt, ms, mbig, mbs;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1v	{ UPDATE mt SET v = v + 100 WHERE id = 1; }
step s1key	{ UPDATE mt SET k = 99 WHERE id = 1; }
step s1src	{ UPDATE mt SET v = v + 100 WHERE id = 1; UPDATE ms SET w = w + 1000 WHERE k = 1; }
step s1del	{ DELETE FROM mt WHERE id = 1; }
step s1mid	{ UPDATE mt SET v = v + 100 WHERE id = 5; }
step s1big	{ UPDATE mbig SET v = v + 1 WHERE id = 777; }
step s1c	{ COMMIT; }

session s2
setup
{
 SET enable_hashjoin = off;
 SET enable_nestloop = off;
 SET enable_indexscan = off;
 SET enable_bitmapscan = off;
}
step s2plan	{ EXPLAIN (costs off) UPDATE mt SET v = mt.v * 10 + ms.w FROM ms WHERE mt.k = ms.k; }
step s2upd	{ UPDATE mt SET v = mt.v * 10 + ms.w FROM ms WHERE mt.k = ms.k; }
step s2merge	{ MERGE INTO mt USING ms ON mt.k = ms.k WHEN MATCHED THEN UPDATE SET v = mt.v * 10 + ms.w; }
step s2del	{ DELETE FROM mt USING ms WHERE mt.k = ms.k; }
step s2bigplan	{ SET work_mem = '64kB'; EXPLAIN (costs off) UPDATE mbig SET v = mbig.v + mbs.w FROM mbs WHERE mbig.k = mbs.k; }
step s2big	{ SET work_mem = '64kB'; UPDATE mbig SET v = mbig.v + mbs.w FROM mbs WHERE mbig.k = mbs.k; }
step s2chk	{ SELECT * FROM mt ORDER BY id; }
step s2bigchk	{ SELECT count(*), sum(v), min(v), max(v) FROM mbig; SELECT id, k, v FROM mbig WHERE id IN (776, 777, 778) ORDER BY id; }

session s3
setup
{
 SET enable_hashjoin = off;
 SET enable_nestloop = off;
 SET enable_seqscan = off;
 SET enable_sort = off;
}
step s3plan	{ EXPLAIN (costs off) UPDATE mt SET v = mt.v * 10 + ms.w FROM ms WHERE mt.k = ms.k; }
step s3upd	{ UPDATE mt SET v = mt.v * 10 + ms.w FROM ms WHERE mt.k = ms.k; }

permutation s2plan s3plan s2bigplan
permutation s1v s2upd s1c s2chk
permutation s1key s2upd s1c s2chk
permutation s1src s2upd s1c s2chk
permutation s1v s2merge s1c s2chk
permutation s1del s2del s1c s2chk
permutation s1mid s2upd s1c s2chk
permutation s1v s3upd s1c s2chk
permutation s1big s2big s1c s2bigchk

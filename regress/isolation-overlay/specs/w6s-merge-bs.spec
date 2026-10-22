# M8-WRITE V4R-495: MERGE's NOT MATCHED BY SOURCE actions under a concurrent change (PostgreSQL 18's ExecMergeMatched:
# the recheck restarts the row's action list over the newest target version; a MATCHED row whose update breaks the join
# takes the BY SOURCE actions; a null-extended source row stays null-extended, EvalPlanQualFetchRowMark's no-row, never
# turning NOT MATCHED into MATCHED). The target is scanned in heap order (k 1, 3, 5, 7, then filler), the source hashed.
# The victim (s2) blocks on a target row s1 changed; after s1 commits:
#  (a) s1 raised a BY SOURCE row's v past the first action's qual: the recheck takes the next action (DELETE)
#  (b) s1 deleted a BY SOURCE row: its action finds it gone, nothing runs (no NOT MATCHED BY TARGET action)
#  (c) s1 moved a MATCHED row's key away: the recheck finds no join, the BY SOURCE actions run on the new version
#  (d) s1 moved a BY SOURCE row's key onto the key of the MATCHED row before it (k 1): the recheck's source is empty
#      (the empty row position, not the previous row's source), so the row stays BY SOURCE
#  (e) s1 moved a BY SOURCE row's key onto a source key no target row had (k 2): it stays BY SOURCE
# Expected output is PostgreSQL 18.6's (the C oracle).

setup
{
 CREATE TABLE bt (k int, v int);
 CREATE TABLE bs (k int, w int);
 INSERT INTO bt VALUES (1, 10), (3, 30), (5, 50), (7, 5000);
 INSERT INTO bt SELECT g, 5000 FROM generate_series(1000, 1199) g;
 INSERT INTO bs VALUES (1, 100), (2, 200);
 ANALYZE bt, bs;
}

teardown
{
 DROP TABLE bt, bs;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1v	{ UPDATE bt SET v = v + 1000 WHERE k = 3; }
step s1del	{ DELETE FROM bt WHERE k = 5; }
step s1away	{ UPDATE bt SET k = 11 WHERE k = 1; }
step s1onto	{ UPDATE bt SET k = 1 WHERE k = 5; }
step s1src	{ UPDATE bt SET k = 2 WHERE k = 3; }
step s1c	{ COMMIT; }

session s2
setup
{
 SET enable_mergejoin = off;
 SET enable_nestloop = off;
}
step s2cnt	{ SELECT count(*) FROM bt; SELECT count(*) FROM bs; }
step s2plan	{ EXPLAIN (costs off) MERGE INTO bt t USING bs s ON t.k = s.k WHEN MATCHED THEN UPDATE SET v = t.v + s.w WHEN NOT MATCHED BY SOURCE AND t.v < 1000 THEN UPDATE SET v = t.v - 1 WHEN NOT MATCHED BY SOURCE THEN DELETE; }
step s2merge	{ MERGE INTO bt t USING bs s ON t.k = s.k WHEN MATCHED THEN UPDATE SET v = t.v + s.w WHEN NOT MATCHED BY SOURCE AND t.v < 1000 THEN UPDATE SET v = t.v - 1 WHEN NOT MATCHED BY SOURCE THEN DELETE; }
step s2chk	{ SELECT * FROM bt ORDER BY k, v; }

permutation s2cnt s2plan
permutation s1v s2merge s1c s2chk
permutation s1del s2merge s1c s2chk
permutation s1away s2merge s1c s2chk
permutation s1onto s2merge s1c s2chk
permutation s1src s2merge s1c s2chk

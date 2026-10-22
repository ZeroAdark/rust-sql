# drain A4-MATEND (D2371): a MERGE with MATCHED and NOT MATCHED actions whose join is a Hash Right Join (the small source
# hashed, the target probing: its MATCHED rows come out of the probe, the source rows no target matched out of the drain
# after it) under a concurrent change (PostgreSQL 18's ExecMerge: a MATCHED row whose target version is gone or no longer
# joins at the recheck falls through to the NOT MATCHED actions). The victim (s2) blocks on a target row s1 changed;
# after s1 commits:
#  (a) s1 deleted the matched target row: the source row takes the NOT MATCHED INSERT
#  (b) s1 moved the target row's key away: the same, the moved row stays
#  (c) s1 updated the matched target row's value: the MATCHED UPDATE runs over the newest version
#  (d) s1 inserted the key s2 would insert (a unique index): s2's INSERT waits, then raises the unique violation
#  (e) s1 updated the source row the join read: the recheck reads the version the join read (REFERENCE mark)
#  (f) a DELETE action, and NOT MATCHED BY SOURCE (a Hash Full Join), over a deleted or moved target row
# Expected output is PostgreSQL 18.6's (the C oracle).

setup
{
 CREATE TABLE dt (k int PRIMARY KEY, v int, note text DEFAULT 'dflt');
 CREATE TABLE ds (k int, w int);
 INSERT INTO dt SELECT g, g * 10 FROM generate_series(1, 3000) g;
 INSERT INTO ds VALUES (1, 100), (2, 200), (4000, 400);
 ANALYZE dt, ds;
}

teardown
{
 DROP TABLE dt, ds;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1del	{ DELETE FROM dt WHERE k = 1; }
step s1key	{ UPDATE dt SET k = 5001 WHERE k = 1; }
step s1v	{ UPDATE dt SET v = v + 1000 WHERE k = 1; }
step s1ins	{ INSERT INTO dt VALUES (4000, 44, 's1'); }
step s1src	{ UPDATE dt SET v = v + 7 WHERE k = 2; UPDATE ds SET w = w + 5 WHERE k = 2; }
step s1c	{ COMMIT; }

session s2
setup
{
 SET enable_mergejoin = off;
 SET enable_nestloop = off;
}
step s2plan	{ EXPLAIN (costs off) MERGE INTO dt t USING ds s ON t.k = s.k WHEN MATCHED THEN UPDATE SET v = t.v + s.w WHEN NOT MATCHED THEN INSERT VALUES (s.k, s.w); }
step s2merge	{ MERGE INTO dt t USING ds s ON t.k = s.k WHEN MATCHED THEN UPDATE SET v = t.v + s.w WHEN NOT MATCHED THEN INSERT VALUES (s.k, s.w); }
step s2del	{ MERGE INTO dt t USING ds s ON t.k = s.k WHEN MATCHED AND s.w < 150 THEN DELETE WHEN MATCHED THEN UPDATE SET note = 'm' WHEN NOT MATCHED THEN INSERT VALUES (s.k, s.w, 'ins'); }
step s2bsplan	{ EXPLAIN (costs off) MERGE INTO dt t USING ds s ON t.k = s.k WHEN MATCHED THEN UPDATE SET v = t.v + s.w WHEN NOT MATCHED BY SOURCE AND t.k <= 3 THEN UPDATE SET note = 'bs' WHEN NOT MATCHED THEN INSERT VALUES (s.k, s.w); }
step s2bs	{ MERGE INTO dt t USING ds s ON t.k = s.k WHEN MATCHED THEN UPDATE SET v = t.v + s.w WHEN NOT MATCHED BY SOURCE AND t.k <= 3 THEN UPDATE SET note = 'bs' WHEN NOT MATCHED THEN INSERT VALUES (s.k, s.w); }
step s2chk	{ SELECT * FROM dt WHERE k <= 4 OR k >= 4000 ORDER BY k; }

permutation s2plan s2bsplan
permutation s1del s2merge s1c s2chk
permutation s1key s2merge s1c s2chk
permutation s1v s2merge s1c s2chk
permutation s1ins s2merge s1c s2chk
permutation s1src s2merge s1c s2chk
permutation s1del s2del s1c s2chk
permutation s1v s2del s1c s2chk
permutation s1del s2bs s1c s2chk
permutation s1key s2bs s1c s2chk
permutation s1v s2bs s1c s2chk

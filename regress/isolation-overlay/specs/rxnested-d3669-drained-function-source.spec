# D3669: MERGE with WHEN NOT MATCHED BY SOURCE actions whose source is a function's rows (generate_series, bare or under
# a subquery), the target hashed under the join (a Hash Right / Full Join, the function scan probing; nested loops and
# merge joins off), while another session changes the target row (READ COMMITTED). One heap target, and a partitioned
# one whose leaves' Append is hashed. PostgreSQL 18.6's ExecMergeMatched rechecks a MATCHED row by EvalPlanQual: the
# function's row is the COPY mark's stored row, the target the newest version; then the join condition over that
# version and the ORIGINAL source row picks the MATCHED or the NOT MATCHED BY SOURCE list. A NOT MATCHED BY SOURCE row
# is refetched at its newest version and never becomes MATCHED.
#  (a) a MATCHED row updated in place     (b) a MATCHED row re-keyed off its source row
#  (c) a NOT MATCHED BY SOURCE row re-keyed onto a source row's key
#  (d) a NOT MATCHED INSERT beside them (a Hash Full Join) over a re-keyed MATCHED row
# The EXPLAIN steps pin the plans. Expected output is PostgreSQL 18.6's (the C oracle).
setup
{
 CREATE TABLE fq (k int, hits int NOT NULL DEFAULT 0, note text);
 INSERT INTO fq SELECT i, 0, 'n' || i FROM generate_series(1, 23) i WHERE i % 10 BETWEEN 1 AND 3;
 CREATE TABLE fp (k int, hits int NOT NULL DEFAULT 0, note text) PARTITION BY RANGE (k);
 CREATE TABLE fp1 PARTITION OF fp FOR VALUES FROM (0) TO (10);
 CREATE TABLE fp2 PARTITION OF fp FOR VALUES FROM (10) TO (20);
 CREATE TABLE fp3 PARTITION OF fp FOR VALUES FROM (20) TO (1000);
 INSERT INTO fp SELECT i, 0, 'n' || i FROM generate_series(1, 23) i WHERE i % 10 BETWEEN 1 AND 3;
 ANALYZE fq, fp;
}

teardown
{
 DROP TABLE fq, fp;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1qv	{ UPDATE fq SET hits = 7 WHERE k = 12; }
step s1qkey	{ UPDATE fq SET k = 14 WHERE k = 12; }
step s1qnkey	{ UPDATE fq SET k = 12 WHERE k = 13; }
step s1pv	{ UPDATE fp SET hits = 7 WHERE k = 12; }
step s1pkey	{ UPDATE fp SET k = 14 WHERE k = 12; }
step s1pnkey	{ UPDATE fp SET k = 12 WHERE k = 13; }
step s1c	{ COMMIT; }

session s2
setup		{ SET enable_nestloop = off; SET enable_mergejoin = off; }
step s2qexp	{ EXPLAIN (COSTS OFF) MERGE INTO fq t USING generate_series(2, 602, 10) g ON t.k = g WHEN MATCHED THEN UPDATE SET hits = t.hits + 100 + g WHEN NOT MATCHED BY SOURCE AND t.k < 15 THEN UPDATE SET note = 'b' || t.hits; }
step s2q	{ MERGE INTO fq t USING generate_series(2, 602, 10) g ON t.k = g WHEN MATCHED THEN UPDATE SET hits = t.hits + 100 + g WHEN NOT MATCHED BY SOURCE AND t.k < 15 THEN UPDATE SET note = 'b' || t.hits; }
step s2qs	{ MERGE INTO fq t USING (SELECT g AS k, g % 7 AS w FROM generate_series(1, 60) g) s ON t.k = s.k WHEN MATCHED AND t.k % 10 = 2 THEN UPDATE SET hits = t.hits + 100 + s.w WHEN NOT MATCHED BY SOURCE THEN UPDATE SET note = 'b' || t.hits; }
step s2pexp	{ EXPLAIN (COSTS OFF) MERGE INTO fp t USING (SELECT g AS k, g % 7 AS w FROM generate_series(2, 602, 10) g) s ON t.k = s.k WHEN MATCHED THEN UPDATE SET hits = t.hits + 100 + s.w WHEN NOT MATCHED BY SOURCE AND t.k < 15 THEN UPDATE SET note = 'b' || t.hits WHEN NOT MATCHED BY SOURCE THEN DELETE; }
step s2p	{ MERGE INTO fp t USING (SELECT g AS k, g % 7 AS w FROM generate_series(2, 602, 10) g) s ON t.k = s.k WHEN MATCHED THEN UPDATE SET hits = t.hits + 100 + s.w WHEN NOT MATCHED BY SOURCE AND t.k < 15 THEN UPDATE SET note = 'b' || t.hits WHEN NOT MATCHED BY SOURCE THEN DELETE; }
step s2pfexp	{ EXPLAIN (COSTS OFF) MERGE INTO fp t USING generate_series(2, 602, 10) g ON t.k = g WHEN MATCHED THEN UPDATE SET hits = t.hits + 100 + g WHEN NOT MATCHED BY SOURCE THEN UPDATE SET note = 'b' || t.hits WHEN NOT MATCHED AND g < 40 THEN INSERT VALUES (g + 40, g, 'i' || g); }
step s2pf	{ MERGE INTO fp t USING generate_series(2, 602, 10) g ON t.k = g WHEN MATCHED THEN UPDATE SET hits = t.hits + 100 + g WHEN NOT MATCHED BY SOURCE THEN UPDATE SET note = 'b' || t.hits WHEN NOT MATCHED AND g < 40 THEN INSERT VALUES (g + 40, g, 'i' || g); }
step s2qsel	{ SELECT k, hits, note FROM fq ORDER BY k, note; }
step s2psel	{ SELECT tableoid::regclass, k, hits, note FROM fp ORDER BY k, note; }

permutation s2qexp s1qv s2q s1c s2qsel
permutation s1qkey s2q s1c s2qsel
permutation s1qnkey s2q s1c s2qsel
permutation s1qkey s2qs s1c s2qsel
permutation s2pexp s1pv s2p s1c s2psel
permutation s1pkey s2p s1c s2psel
permutation s1pnkey s2p s1c s2psel
permutation s2pfexp s1pkey s2pf s1c s2psel

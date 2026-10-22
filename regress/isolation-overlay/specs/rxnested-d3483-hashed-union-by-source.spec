# D3483: MERGE with WHEN NOT MATCHED BY SOURCE actions into a partitioned table whose leaves are hashed under the join
# (a Hash Right Join over the Append of the leaves' scans, the larger source probing; nested loops and merge joins off),
# while another session changes the target row (READ COMMITTED). PostgreSQL 18.6's ExecMergeMatched rechecks a MATCHED
# row by EvalPlanQual (the row's own leaf at its newest version, every other leaf empty) and then evaluates the join
# condition over the newest version and the ORIGINAL source row: still true, the MATCHED action; false, the NOT MATCHED
# BY SOURCE list over the newest version. A NOT MATCHED BY SOURCE row is refetched at its newest version and its action
# quals evaluated again; it never becomes MATCHED. A row moved to another leaf raises the moved-partition error.
#  (a) a MATCHED row updated in place     (b) a MATCHED row re-keyed off its source row
#  (c) a NOT MATCHED BY SOURCE row updated in place, re-keyed onto a source row's key, and re-keyed past its action's qual
#  (d) a MATCHED row deleted              (e) NOT MATCHED BY SOURCE actions alone over a re-keyed row
#  (f) a MATCHED row moved to another leaf
#  (g) a NOT MATCHED INSERT beside them (a Hash Full Join): a MATCHED row re-keyed off its source row takes the BY SOURCE
#      action, and its source row the NOT MATCHED one       (h) RETURNING over a MATCHED row updated in place
# The EXPLAIN step pins the plan. Expected output is PostgreSQL 18.6's (the C oracle).
setup
{
 CREATE TABLE hp (k int, hits int NOT NULL DEFAULT 0, note text) PARTITION BY RANGE (k);
 CREATE TABLE hp1 PARTITION OF hp FOR VALUES FROM (0) TO (10);
 CREATE TABLE hp2 PARTITION OF hp FOR VALUES FROM (10) TO (20);
 CREATE TABLE hp3 PARTITION OF hp FOR VALUES FROM (20) TO (1000);
 INSERT INTO hp SELECT i, 0, 'n' || i FROM generate_series(1, 23) i WHERE i % 10 BETWEEN 1 AND 3;
 CREATE TABLE hs (k int, w int);
 INSERT INTO hs VALUES (2, 1), (12, 2), (22, 3);
 INSERT INTO hs SELECT i, 0 FROM generate_series(200, 259) i;
 ANALYZE hp, hs;
}

teardown
{
 DROP TABLE hp, hs;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1v	{ UPDATE hp SET hits = 7 WHERE k = 12; }
step s1key	{ UPDATE hp SET k = 14 WHERE k = 12; }
step s1nv	{ UPDATE hp SET hits = 8 WHERE k = 13; }
step s1nkey	{ UPDATE hp SET k = 12 WHERE k = 13; }
step s1nfar	{ UPDATE hp SET k = 16 WHERE k = 13; }
step s1del	{ DELETE FROM hp WHERE k = 22; }
step s1mv	{ UPDATE hp SET k = 15 WHERE k = 2; }
step s1c	{ COMMIT; }

session s2
setup		{ SET enable_nestloop = off; SET enable_mergejoin = off; }
step s2exp	{ EXPLAIN (COSTS OFF) MERGE INTO hp t USING hs s ON t.k = s.k WHEN MATCHED THEN UPDATE SET hits = t.hits + 100 + s.w WHEN NOT MATCHED BY SOURCE AND t.k < 15 THEN UPDATE SET note = 'b' || t.hits WHEN NOT MATCHED BY SOURCE THEN DELETE; }
step s2m	{ MERGE INTO hp t USING hs s ON t.k = s.k WHEN MATCHED THEN UPDATE SET hits = t.hits + 100 + s.w WHEN NOT MATCHED BY SOURCE AND t.k < 15 THEN UPDATE SET note = 'b' || t.hits WHEN NOT MATCHED BY SOURCE THEN DELETE; }
step s2fexp	{ EXPLAIN (COSTS OFF) MERGE INTO hp t USING hs s ON t.k = s.k WHEN MATCHED THEN UPDATE SET hits = t.hits + 100 + s.w WHEN NOT MATCHED BY SOURCE THEN UPDATE SET note = 'b' || t.hits WHEN NOT MATCHED AND s.k < 203 THEN INSERT VALUES (s.k % 100 + 30, s.w, 'i' || s.k); }
step s2f	{ MERGE INTO hp t USING hs s ON t.k = s.k WHEN MATCHED THEN UPDATE SET hits = t.hits + 100 + s.w WHEN NOT MATCHED BY SOURCE THEN UPDATE SET note = 'b' || t.hits WHEN NOT MATCHED AND s.k < 203 THEN INSERT VALUES (s.k % 100 + 30, s.w, 'i' || s.k); }
step s2r	{ MERGE INTO hp t USING hs s ON t.k = s.k WHEN MATCHED THEN UPDATE SET hits = t.hits + 100 + s.w WHEN NOT MATCHED BY SOURCE AND t.k = 13 THEN UPDATE SET note = 'b' || t.hits RETURNING merge_action(), t.k, t.hits, t.note, s.k, s.w; }
step s2b	{ MERGE INTO hp t USING hs s ON t.k = s.k WHEN NOT MATCHED BY SOURCE AND t.k < 15 THEN UPDATE SET hits = t.hits + 1; }
step s2sel	{ SELECT tableoid::regclass, k, hits, note FROM hp ORDER BY k, note; }

permutation s2exp s1v s2m s1c s2sel
permutation s1key s2m s1c s2sel
permutation s1nv s2m s1c s2sel
permutation s1nkey s2m s1c s2sel
permutation s1nfar s2m s1c s2sel
permutation s1del s2m s1c s2sel
permutation s1nkey s2b s1c s2sel
permutation s1mv s2m s1c s2sel
permutation s2fexp s1key s2f s1c s2sel
permutation s1v s2f s1c s2sel
permutation s1v s2r s1c s2sel

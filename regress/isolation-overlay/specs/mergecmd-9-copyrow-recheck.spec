# mergecmd 9 (COPYROW): a MERGE over a nested loop whose outer is a COPY-marked source (a VALUES list, or a subquery the
# planner keeps: OFFSET 0) and whose matched row another session updates concurrently (READ COMMITTED). The target has
# 2000 rows, so the plan is a Nested Loop (Left) Join: the source outer, the target's index scan inner. PostgreSQL
# 18.6's ExecMergeMatched rechecks the newest version under EvalPlanQual with the source row taken from its wholerow junk
# (EvalPlanQualFetchRowMark's copy leg), never the source run again:
#  (a) a one-row subquery source, s1 updates the matched row: the UPDATE action reads the newest version
#  (b) the same source, s1 moves the row's key away: the newest version no longer joins, the NOT MATCHED INSERT runs
#  (c) a three-row VALUES source, s1 updates one matched row: that row's UPDATE reads the newest version and the stored
#      VALUES row (60), the other two rows update once
#  (d) the same source, s1 moves the key away: the NOT MATCHED INSERT runs for that VALUES row alone
#  (e) the same source, s1 deletes the row: the NOT MATCHED INSERT runs
#  (f) s1 aborts: the original version is updated (control)
#  (g) a MATCHED-only MERGE (a Nested Loop inner join) over the three-row VALUES, with a DELETE whose qual reads the
#      source row: the newest version is deleted
#  (h) a subquery reading a table (OFFSET 0), s1 updates one matched row: its stored row, the newest version
#  (i) the same source, s1 moves the key away: the NOT MATCHED INSERT runs for that stored row alone
#  (j) a volatile constant subquery, s1 updates the matched row: its stored row, the newest version
#  (k) a three-row target hashed on the join's nullable side (nested loops and merge joins off), probed by a subquery over
#      generate_series(1, 50): s1 updates a matched row, its UPDATE reads the newest version and the stored row
#  (l) the same target probed by an eight-row VALUES list, s1 deletes a matched row: the NOT MATCHED INSERT runs
# Expected output is PostgreSQL 18.6's (the C oracle).

setup
{
 CREATE TABLE ct (k int PRIMARY KEY, v int, note text);
 INSERT INTO ct SELECT g, g, 'i' FROM generate_series(1, 2000) g;
 CREATE TABLE ct3 (k int PRIMARY KEY, v int, note text);
 INSERT INTO ct3 VALUES (4, 4, 'i'), (5, 5, 'i'), (6, 6, 'i');
 CREATE TABLE src (k int, v int);
 INSERT INTO src VALUES (4, 400), (5, 500), (6, 600);
 ANALYZE ct, ct3, src;
}

teardown
{
 DROP TABLE ct, ct3, src;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1upd	{ UPDATE ct SET v = v + 1000 WHERE k = 5; }
step s1key	{ UPDATE ct SET k = 5000 WHERE k = 5; }
step s1del	{ DELETE FROM ct WHERE k = 5; }
step s1upd3	{ UPDATE ct3 SET v = v + 1000 WHERE k = 5; }
step s1del3	{ DELETE FROM ct3 WHERE k = 5; }
step s1c	{ COMMIT; }
step s1a	{ ROLLBACK; }

session s2
step s2sub	{ MERGE INTO ct t USING (SELECT 5 AS k, 50 AS v OFFSET 0) s ON t.k = s.k WHEN MATCHED THEN UPDATE SET v = t.v + s.v, note = 'u' WHEN NOT MATCHED THEN INSERT VALUES (s.k, s.v, 'nm'); }
step s2vals	{ MERGE INTO ct t USING (VALUES (4, 40), (5, 60), (6, 70)) s(k, v) ON t.k = s.k WHEN MATCHED THEN UPDATE SET v = t.v + s.v, note = 'u' WHEN NOT MATCHED THEN INSERT VALUES (s.k, s.v, 'nm'); }
step s2valm	{ MERGE INTO ct t USING (VALUES (4, 40), (5, 60), (6, 70)) s(k, v) ON t.k = s.k WHEN MATCHED AND s.v = 60 THEN DELETE WHEN MATCHED THEN UPDATE SET note = 'm'; }
step s2tab	{ MERGE INTO ct t USING (SELECT k, v FROM src OFFSET 0) s ON t.k = s.k WHEN MATCHED THEN UPDATE SET v = t.v + s.v, note = 'tab' WHEN NOT MATCHED THEN INSERT VALUES (s.k, s.v, 'nm'); }
step s2vol	{ MERGE INTO ct t USING (SELECT 5 AS k, 50 + (random() * 0)::int AS v OFFSET 0) s ON t.k = s.k WHEN MATCHED THEN UPDATE SET v = t.v + s.v, note = 'vol' WHEN NOT MATCHED THEN INSERT VALUES (s.k, s.v, 'nm'); }
step s2sel	{ SELECT * FROM ct WHERE k IN (4, 5, 6, 5000) ORDER BY k; }

session s3
setup		{ SET enable_nestloop = off; SET enable_mergejoin = off; }
step s3sub	{ MERGE INTO ct3 t USING (SELECT g AS k, g * 10 AS v FROM generate_series(1, 50) g OFFSET 0) s ON t.k = s.k WHEN MATCHED THEN UPDATE SET v = t.v + s.v, note = 'h' WHEN NOT MATCHED THEN INSERT VALUES (s.k, s.v, 'nm'); }
step s3val	{ MERGE INTO ct3 t USING (VALUES (4, 40), (5, 50), (6, 60), (7, 70), (8, 80), (9, 90), (10, 100), (11, 110)) s(k, v) ON t.k = s.k WHEN MATCHED THEN UPDATE SET v = t.v + s.v, note = 'h' WHEN NOT MATCHED THEN INSERT VALUES (s.k, s.v, 'nm'); }
step s3sel	{ SELECT * FROM ct3 WHERE k IN (4, 5, 6, 7) ORDER BY k; }

permutation s1upd s2sub s1c s2sel
permutation s1key s2sub s1c s2sel
permutation s1upd s2vals s1c s2sel
permutation s1key s2vals s1c s2sel
permutation s1del s2vals s1c s2sel
permutation s1upd s2vals s1a s2sel
permutation s1upd s2valm s1c s2sel
permutation s1upd s2tab s1c s2sel
permutation s1key s2tab s1c s2sel
permutation s1upd s2vol s1c s2sel
permutation s1upd3 s3sub s1c s3sel
permutation s1del3 s3val s1c s3sel

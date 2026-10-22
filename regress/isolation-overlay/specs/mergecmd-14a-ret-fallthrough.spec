# mergecmd 14a: a MERGE with RETURNING whose matched row another session deletes concurrently (READ COMMITTED).
# PostgreSQL 18.6's ExecMergeMatched waits for s1, meets TM_Deleted and falls through to the NOT MATCHED actions; the
# INSERT it runs returns its row with merge_action() 'INSERT' (ExecMergeNotMatched -> ExecInsert's RETURNING).
#  (a) the target hashed on the join's nullable side (a three-row target, a forty-seven-row source probing it)
#  (b) a one-row source (a Nested Loop Left Join: the Result outer, the target's scan inner)
#  (c) s1 aborts: the UPDATE action's new row is returned (control)
# The action quals keep one returned row per statement. Expected output is PostgreSQL 18.6's (the C oracle).

setup
{
 CREATE TABLE rt (k int PRIMARY KEY, v int, note text);
 CREATE TABLE rs (k int, v int);
 INSERT INTO rt VALUES (4, 4, 'i'), (5, 5, 'i'), (6, 6, 'i');
 INSERT INTO rs SELECT g, g * 10 FROM generate_series(4, 50) g;
 ANALYZE rt, rs;
}

teardown
{
 DROP TABLE rt, rs;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1del4	{ DELETE FROM rt WHERE k = 4; }
step s1del5	{ DELETE FROM rt WHERE k = 5; }
step s1c	{ COMMIT; }
step s1a	{ ROLLBACK; }

session s2
setup		{ SET enable_nestloop = off; SET enable_mergejoin = off; }
step s2hash	{ MERGE INTO rt t USING rs s ON t.k = s.k WHEN MATCHED AND t.k = 4 THEN UPDATE SET v = s.v, note = 'u' WHEN NOT MATCHED AND s.k = 4 THEN INSERT VALUES (s.k, s.v + 1, 'nm') RETURNING merge_action(), t.*; }
step s2sel	{ SELECT * FROM rt ORDER BY k; }

session s3
step s3nest	{ MERGE INTO rt t USING (SELECT 5 AS k, 50 AS v) s ON t.k = s.k WHEN MATCHED THEN UPDATE SET v = s.v, note = 'u' WHEN NOT MATCHED THEN INSERT VALUES (s.k, s.v + 1, 'nm') RETURNING merge_action(), t.*; }

permutation s1del4 s2hash s1c s2sel
permutation s1del5 s3nest s1c s2sel
permutation s1del4 s2hash s1a s2sel

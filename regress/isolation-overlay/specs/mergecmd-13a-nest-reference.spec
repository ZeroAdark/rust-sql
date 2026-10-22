# mergecmd 13a (gen mergecmd source-cte / empty-source rows): a MERGE whose source is a plain table (its row a
# REFERENCE mark) over a nested loop, the source's seq scan outer and the target's primary-key index scan inner, while
# another session changes the matched row (READ COMMITTED). PostgreSQL 18.6's ExecMergeMatched rechecks the newest
# version under EvalPlanQual with the source row refetched at its ctid under SnapshotAny (EvalPlanQualFetchRowMark):
# the version the join read, never its newest, and never any other source row. The source holds a row whose key is
# the one the matched row moves to, so a recheck that read the whole source again would join it.
#  (a) s1 updates the matched row: the UPDATE reads the newest version
#  (b) s1 moves the row's key to the other source row's key: the recheck finds no match, the NOT MATCHED INSERT runs
#  (c) s1 deletes the row: the NOT MATCHED INSERT runs
#  (d) s1 also updates the source row: the recheck reads the version the join read
#  (e) the MATCHED action deletes: the newest version is deleted
#  (f) s1 aborts: the original version is updated (control)
#  (g) mergecmd 13b: the source a subquery with ORDER BY and LIMIT (its row a COPY mark, the LIMIT under the nest's
#      outer): updated and moved rows, the recheck entering with the stored row (the LIMIT is not run again)
# Expected output is PostgreSQL 18.6's (the C oracle).
setup
{
 CREATE TABLE nr (k int PRIMARY KEY, v text);
 INSERT INTO nr SELECT i, i || ' v' FROM generate_series(1, 2000) i;
 CREATE TABLE nrs (k int, w text);
 INSERT INTO nrs VALUES (13, 'a'), (3000, 'b'), (14, 'c');
 ANALYZE nr; ANALYZE nrs;
}

teardown
{
 DROP TABLE nr, nrs;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1upd	{ UPDATE nr SET v = v || ' s1' WHERE k = 13; }
step s1key	{ UPDATE nr SET k = 3000 WHERE k = 13; }
step s1del	{ DELETE FROM nr WHERE k = 13; }
step s1src	{ UPDATE nrs SET w = 'z' WHERE k = 13; }
step s1c	{ COMMIT; }
step s1a	{ ROLLBACK; }

session s2
step s2exp	{ EXPLAIN (COSTS OFF) MERGE INTO nr USING nrs ON nr.k = nrs.k WHEN MATCHED THEN UPDATE SET v = nr.v || ' + ' || nrs.w WHEN NOT MATCHED THEN INSERT VALUES (nrs.k + 5000, nrs.w); }
step s2mrg	{ MERGE INTO nr USING nrs ON nr.k = nrs.k WHEN MATCHED THEN UPDATE SET v = nr.v || ' + ' || nrs.w WHEN NOT MATCHED THEN INSERT VALUES (nrs.k + 5000, nrs.w); }
step s2del	{ MERGE INTO nr USING nrs ON nr.k = nrs.k WHEN MATCHED THEN DELETE WHEN NOT MATCHED THEN INSERT VALUES (nrs.k + 5000, nrs.w); }
step s2lim	{ MERGE INTO nr USING (SELECT * FROM nrs ORDER BY k LIMIT 2) s ON nr.k = s.k WHEN MATCHED THEN UPDATE SET v = nr.v || ' + ' || s.w WHEN NOT MATCHED THEN INSERT VALUES (s.k + 5000, s.w); }
step s2sel	{ SELECT * FROM nr WHERE k IN (13, 14, 3000, 5013, 5014, 8000) ORDER BY k; }

permutation s2exp s1upd s2mrg s1c s2sel
permutation s1key s2mrg s1c s2sel
permutation s1del s2mrg s1c s2sel
permutation s1upd s1src s2mrg s1c s2sel
permutation s1upd s2del s1c s2sel
permutation s1upd s2mrg s1a s2sel
permutation s1upd s2lim s1c s2sel
permutation s1key s2lim s1c s2sel

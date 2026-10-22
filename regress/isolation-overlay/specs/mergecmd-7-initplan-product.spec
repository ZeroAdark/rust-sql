# mergecmd landing 7 (D2028 by the effects licence): a MERGE over a nested loop (a one-row Result outer, the target's
# index scan inner) whose actions read an InitPlan that reads a product (a MATERIALIZED CTE's spool, a Sort inside the
# InitPlan), while another session changes the matched row (READ COMMITTED). PostgreSQL 18.6's ExecMergeMatched rechecks
# the newest version under EvalPlanQual: the InitPlan's param is the one this execution computed (EvalPlanQualStart
# copies it; one an action reads is evaluated lazily, once, in the main estate), so it never runs again and never sees
# the other session's commit. The recheck here re-runs a non-acting InitPlan under the statement's snapshot, which can
# not differ. A VOLATILE CTE under an InitPlan still refuses by D2028's name (arm b's residual file):
#  (a) SET reads an InitPlan over a CTE, s1 updates the matched row: the UPDATE reads the newest version, the CTE's max
#  (b) the same, s1 moves the row's key away: the NOT MATCHED INSERT runs
#  (c) the same, s1 updates the matched row and the CTE's table in one transaction: the InitPlan's value is the
#      statement snapshot's (the old max), as PostgreSQL's copied param
#  (d) SET reads an InitPlan over a Sort (ORDER BY .. LIMIT 1), s1 updates the matched row and the sorted table
#  (e) the NOT MATCHED INSERT reads an InitPlan over a CTE that the first attempt (MATCHED) never read, s1 deletes the
#      row: the INSERT evaluates it then, under the statement's snapshot
#  (f) s1 aborts: the original version is updated (control)
#  (g) a VOLATILE CTE (nextval) read by the SET's InitPlan, s1 updates the matched row: nextval runs once (PG copies the
#      param); refused by D2028's name under the fallback off
#  (h) mergecmd 7b: the ON clause's conjunct `t.v < (SELECT 1000 / w FROM pr WHERE k = 3)` (an InitPlan that can
#      raise, read by the target scan's filter, so made before the matched row reaches the write), s1 changes the row's
#      note: the recheck's newest version still passes the filter and is updated
#  (i) the same, s1 moves v past the InitPlan's value: the newest version fails the filter, the NOT MATCHED INSERT runs
#  (j) the same, s1 also raises the InitPlan's table in its transaction: the value stays the statement snapshot's
#  (k) s1 changes the row's note and the InitPlan's table: the statement snapshot's value (33) still passes the newest
#      version (a re-run InitPlan over the new table, 0, would not)
# Expected output is PostgreSQL 18.6's (the C oracle).

setup
{
 CREATE TABLE ct (k int PRIMARY KEY, v int, note text);
 INSERT INTO ct SELECT g, g, 'i' FROM generate_series(1, 2000) g;
 CREATE TABLE pr (k int, w int);
 INSERT INTO pr SELECT g, g * 10 FROM generate_series(1, 20) g;
 CREATE SEQUENCE ipq;
 ANALYZE ct, pr;
}

teardown
{
 DROP TABLE ct, pr;
 DROP SEQUENCE ipq;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1upd	{ UPDATE ct SET v = v + 1000 WHERE k = 5; }
step s1key	{ UPDATE ct SET k = 5000 WHERE k = 5; }
step s1del	{ DELETE FROM ct WHERE k = 5; }
step s1both	{ UPDATE ct SET v = v + 1000 WHERE k = 5; UPDATE pr SET w = w + 7000; }
step s1note	{ UPDATE ct SET note = 'x' WHERE k = 5; }
step s1both2	{ UPDATE ct SET note = 'y' WHERE k = 5; UPDATE pr SET w = w + 7000; }
step s1c	{ COMMIT; }
step s1a	{ ROLLBACK; }

session s2
step s2cte	{ WITH c AS MATERIALIZED (SELECT k, w FROM pr) MERGE INTO ct t USING (SELECT 5 AS k) s ON t.k = s.k WHEN MATCHED THEN UPDATE SET v = t.v + (SELECT max(w) FROM c), note = 'cte' WHEN NOT MATCHED THEN INSERT VALUES (s.k, 0, 'nm'); }
step s2sort	{ MERGE INTO ct t USING (SELECT 5 AS k) s ON t.k = s.k WHEN MATCHED THEN UPDATE SET v = t.v + (SELECT w FROM pr ORDER BY w DESC LIMIT 1), note = 'sort' WHEN NOT MATCHED THEN INSERT VALUES (s.k, 0, 'nm'); }
step s2ins	{ WITH c AS MATERIALIZED (SELECT k, w FROM pr) MERGE INTO ct t USING (SELECT 5 AS k) s ON t.k = s.k WHEN MATCHED THEN UPDATE SET note = 'm' WHEN NOT MATCHED THEN INSERT VALUES (s.k, (SELECT sum(w) FROM c), 'nm cte'); }
step s2vol	{ WITH c AS MATERIALIZED (SELECT nextval('ipq') AS n) MERGE INTO ct t USING (SELECT 5 AS k) s ON t.k = s.k WHEN MATCHED THEN UPDATE SET v = t.v + (SELECT n FROM c), note = 'vol' WHEN NOT MATCHED THEN INSERT VALUES (s.k, 0, 'nm'); }
step s2flt	{ MERGE INTO ct t USING (SELECT 5 AS k) s ON t.k = s.k AND t.v < (SELECT 1000 / w FROM pr WHERE k = 3) WHEN MATCHED THEN UPDATE SET v = t.v + 1, note = 'flt' WHEN NOT MATCHED THEN INSERT VALUES (s.k + 10000, 0, 'nm flt'); }
step s2sel	{ SELECT * FROM ct WHERE k IN (5, 5000) ORDER BY k; }
step s2seq	{ SELECT nextval('ipq'); }
step s2sel2	{ SELECT * FROM ct WHERE k IN (5, 10005) ORDER BY k; }

permutation s1upd s2cte s1c s2sel
permutation s1key s2cte s1c s2sel
permutation s1both s2cte s1c s2sel
permutation s1both s2sort s1c s2sel
permutation s1del s2ins s1c s2sel
permutation s1upd s2cte s1a s2sel
permutation s1upd s2vol s1c s2sel s2seq
permutation s1note s2flt s1c s2sel2
permutation s1upd s2flt s1c s2sel2
permutation s1both s2flt s1c s2sel2
permutation s1both2 s2flt s1c s2sel2

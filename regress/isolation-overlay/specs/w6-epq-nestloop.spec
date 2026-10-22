# W6-MERGE-NEST-D1933 (M8-WRITE, D1933): MERGE over a nested loop's rows under a concurrent change (PostgreSQL 18.6's
# ExecMergeMatched on TM_Updated: lock the newest version, EvalPlanQual over the plan with the target's scan returning that
# version alone, the actions restarted from the first; TM_Deleted: the NOT MATCHED actions over the source row). The
# source is a pulled-up constant, so the planner joins a one-row Result (outer) to the target's scan (inner): nt's primary
# key index (Index Cond id = 2), ns's seq scan (Filter id = 2). The victim (s2) blocks on the row s1 changed; after s1
# commits:
#  up    s1 raised val: the recheck joins the new version, SET computed from it with the original source values
#  key   s1 moved the key away: LEFT inserts the source row (NOT MATCHED), INNER does nothing
#  del   s1 deleted the row: LEFT inserts the source row, INNER does nothing
#  big   s1 raised val past the first action's qual: the recheck takes the next action
#  ret   RETURNING after the recheck: the version written
#  iwhen an InitPlan that would raise (21000), read only by a WHEN qual that neither attempt reaches: no error (lazy)
#  iset  an InitPlan in SET under a concurrent change to its table: the value the first attempt read (PG copies it)
# (each LEFT: a NOT MATCHED action keeps the Result outer, so the plan is the nest; a MATCHED-only MERGE over a pulled-up
# constant plans the target's own scan alone, the trunk's form)
#  vq / vset / vwhen  a VOLATILE call in the source's qual, in SET, in a WHEN qual: once at the first attempt and once
#        in the recheck (PG's EvalPlanQual runs the Result again and restarts the actions), never at the resume
#  rr    REPEATABLE READ / SERIALIZABLE: 40001
# Refused by name under the fallback off (arm b, regress/known-residual-v4/M4/isolation/w6-epq-nestloop): a VOLATILE
# InitPlan (D2028: the recheck would run it again), one that can raise read under the write (init: the index key; an OR
# arm the first attempt skipped, which PG's EvalPlanQualStart forces, 21000, is not a step here: the fallback's own
# executor answers it without the error, D2052), a subquery source with a
# VOLATILE target list (a COPY mark: PG hands the stored row over and calls nextval once), a VALUES outer (F2), a
# BitmapOr inner. Expected output is PostgreSQL 18.6's.

setup
{
 CREATE TABLE nt (id int PRIMARY KEY, val int, note text);
 INSERT INTO nt SELECT g, g * 10, 'n' || g FROM generate_series(1, 1000) g;
 CREATE TABLE ns (id int, val int, note text);
 INSERT INTO ns SELECT g, g * 10, 's' || g FROM generate_series(1, 1000) g;
 CREATE TABLE nb (id int, k2 int, val int);
 INSERT INTO nb SELECT g, g + 5000, 0 FROM generate_series(1, 2000) g;
 CREATE INDEX nb_id ON nb (id);
 CREATE INDEX nb_k2 ON nb (k2);
 CREATE TABLE nr (id int PRIMARY KEY, w int);
 INSERT INTO nr SELECT g, g * 100 FROM generate_series(1, 300) g;
 CREATE TABLE w6k (k int);
 INSERT INTO w6k VALUES (1), (2);
 CREATE SEQUENCE w6q;
 CREATE FUNCTION w6_bump(int) RETURNS boolean LANGUAGE plpgsql VOLATILE AS $$ BEGIN PERFORM nextval('w6q'); RETURN true; END $$;
 ANALYZE nt, ns, nb, nr, w6k;
}

teardown
{
 DROP TABLE nt, ns, nb, nr, w6k;
 DROP FUNCTION w6_bump(int);
 DROP SEQUENCE w6q;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1up	{ UPDATE nt SET val = val + 100 WHERE id = 2; }
step s1key	{ UPDATE nt SET id = 2000 WHERE id = 2; }
step s1del	{ DELETE FROM nt WHERE id = 2; }
step s1big	{ UPDATE nt SET val = 500 WHERE id = 2; }
step s1hup	{ UPDATE ns SET val = val + 100 WHERE id = 2; }
step s1hkey	{ UPDATE ns SET id = 2000 WHERE id = 2; }
step s1bup	{ UPDATE nb SET val = val + 100 WHERE id = 2; }
step s1nr	{ UPDATE nr SET w = 999 WHERE id = 2; }
step s1c	{ COMMIT; }

session s2
step s2plan	{ EXPLAIN (COSTS OFF) MERGE INTO nt t USING (SELECT 2 AS id, 7 AS d) s ON t.id = s.id WHEN MATCHED THEN UPDATE SET val = t.val + s.d WHEN NOT MATCHED THEN INSERT VALUES (s.id, s.d, 'new'); EXPLAIN (COSTS OFF) MERGE INTO ns t USING (SELECT 2 AS id, 7 AS d) s ON t.id = s.id WHEN MATCHED THEN UPDATE SET val = t.val + s.d; EXPLAIN (COSTS OFF) MERGE INTO nb t USING (SELECT 2 AS id, 5010 AS k2) s ON t.id = s.id OR t.k2 = s.k2 WHEN MATCHED THEN UPDATE SET val = t.val + 1; }
step s2left	{ MERGE INTO nt t USING (SELECT 2 AS id, 7 AS d) s ON t.id = s.id WHEN MATCHED THEN UPDATE SET val = t.val + s.d, note = 'merged' WHEN NOT MATCHED THEN INSERT VALUES (s.id, s.d, 'inserted'); }
step s2inner	{ MERGE INTO nt t USING (SELECT 2 AS id, 7 AS d) s ON t.id = s.id WHEN MATCHED THEN UPDATE SET val = t.val + s.d, note = 'merged inner'; }
step s2mdel	{ MERGE INTO nt t USING (SELECT 2 AS id) s ON t.id = s.id WHEN MATCHED THEN DELETE WHEN NOT MATCHED THEN INSERT VALUES (s.id, -1, 'inserted'); }
step s2when	{ MERGE INTO nt t USING (SELECT 2 AS id) s ON t.id = s.id WHEN MATCHED AND t.val < 100 THEN UPDATE SET note = 'small' WHEN MATCHED THEN UPDATE SET note = 'big'; }
step s2ret	{ MERGE INTO nt t USING (SELECT 2 AS id, 7 AS d) s ON t.id = s.id WHEN MATCHED THEN UPDATE SET val = t.val + s.d RETURNING merge_action(), old.val AS old_val, t.*; }
step s2init	{ MERGE INTO nt t USING (SELECT (SELECT id FROM nr WHERE w = 200) AS id, 7 AS d) s ON t.id = s.id WHEN MATCHED THEN UPDATE SET val = t.val + s.d, note = 'init' WHEN NOT MATCHED THEN INSERT VALUES (s.id, s.d, 'init ins'); }
step s2bump	{ MERGE INTO nt t USING (SELECT (SELECT max(k) FROM w6k WHERE w6_bump(k)) AS id) s ON t.id = s.id WHEN MATCHED THEN UPDATE SET note = 'bump'; }
step s2seq	{ MERGE INTO nt t USING (SELECT 2 AS id, nextval('w6q') AS n) s ON t.id = s.id WHEN MATCHED THEN UPDATE SET val = s.n; }
step s2vq	{ MERGE INTO nt t USING (SELECT 2 AS id WHERE w6_bump(2)) s ON t.id = s.id WHEN MATCHED THEN UPDATE SET note = 'volatile source qual'; }
step s2vset	{ MERGE INTO nt t USING (SELECT 2 AS id) s ON t.id = s.id WHEN MATCHED THEN UPDATE SET val = nextval('w6q'); }
step s2vwhen	{ MERGE INTO nt t USING (SELECT 2 AS id) s ON t.id = s.id WHEN MATCHED AND w6_bump(t.id) THEN UPDATE SET note = 'volatile when'; }
step s2iwhen	{ MERGE INTO nt t USING (SELECT 2 AS id) s ON t.id = s.id WHEN MATCHED AND t.val > 100000 AND (SELECT id FROM nr) = 1 THEN UPDATE SET note = 'never' WHEN MATCHED THEN UPDATE SET note = 'iwhen' WHEN NOT MATCHED THEN INSERT VALUES (s.id, 0, 'iwhen ins'); }
step s2iset	{ MERGE INTO nt t USING (SELECT 2 AS id) s ON t.id = s.id WHEN MATCHED THEN UPDATE SET note = 'w' || (SELECT w FROM nr WHERE id = 2) WHEN NOT MATCHED THEN INSERT VALUES (s.id, 0, 'iset ins'); }
step s2vals	{ MERGE INTO nt t USING (VALUES (2, 7), (3, 8)) s(id, d) ON t.id = s.id WHEN MATCHED THEN UPDATE SET val = t.val + s.d WHEN NOT MATCHED THEN INSERT VALUES (s.id, s.d, 'v'); }
step s2hleft	{ MERGE INTO ns t USING (SELECT 2 AS id, 7 AS d) s ON t.id = s.id WHEN MATCHED THEN UPDATE SET val = t.val + s.d, note = 'merged' WHEN NOT MATCHED THEN INSERT VALUES (s.id, s.d, 'inserted'); }
step s2hinner	{ MERGE INTO ns t USING (SELECT 2 AS id, 7 AS d) s ON t.id = s.id WHEN MATCHED THEN UPDATE SET val = t.val + s.d, note = 'merged inner'; }
step s2bmap	{ MERGE INTO nb t USING (SELECT 2 AS id, 5010 AS k2) s ON t.id = s.id OR t.k2 = s.k2 WHEN MATCHED THEN UPDATE SET val = t.val + 1; }
step s2rr	{ BEGIN ISOLATION LEVEL REPEATABLE READ; SELECT count(*) FROM nt; }
step s2sz	{ BEGIN ISOLATION LEVEL SERIALIZABLE; SELECT count(*) FROM nt; }
step s2rb	{ ROLLBACK; }
step s2chk	{ SELECT * FROM nt WHERE id IN (2, 3, 2000) ORDER BY id; }
step s2hchk	{ SELECT * FROM ns WHERE id IN (2, 2000) ORDER BY id, val; }
step s2bchk	{ SELECT * FROM nb WHERE val <> 0 ORDER BY id; }
step s2seqv	{ SELECT nextval('w6q'); }

permutation s2plan
permutation s1up s2left s1c s2chk
permutation s1key s2left s1c s2chk
permutation s1key s2inner s1c s2chk
permutation s1del s2left s1c s2chk
permutation s1del s2inner s1c s2chk
permutation s1up s2mdel s1c s2chk
permutation s1key s2mdel s1c s2chk
permutation s1big s2when s1c s2chk
permutation s1up s2ret s1c s2chk
permutation s1up s2init s1c s2chk
permutation s1up s2bump s1c s2chk s2seqv
permutation s1up s2seq s1c s2chk s2seqv
permutation s1up s2vals s1c s2chk
permutation s1up s2iwhen s1c s2chk
permutation s1nr s1up s2iset s1c s2chk
permutation s1up s2vq s1c s2chk s2seqv
permutation s1up s2vset s1c s2chk s2seqv
permutation s1up s2vwhen s1c s2chk s2seqv
permutation s1hup s2hleft s1c s2hchk
permutation s1hkey s2hleft s1c s2hchk
permutation s1hkey s2hinner s1c s2hchk
permutation s1bup s2bmap s1c s2bchk
permutation s2rr s1up s2inner s1c s2rb s2chk
permutation s2sz s1up s2left s1c s2rb s2chk

# mergecmd D2042: a MERGE whose target has a BEFORE ROW UPDATE / DELETE trigger, and whose matched row another session
# deletes concurrently (READ COMMITTED). PostgreSQL 18.6's ExecMergeMatched: the action's prologue (ExecBRUpdateTriggers
# / ExecBRDeleteTriggers -> GetTupleForTrigger) waits for s1, then meets TM_Deleted and takes the source row as no longer
# matched (*matched = false): the NOT MATCHED actions run for it. Before the fix Gravity's prologue skipped the row.
#  (a) a one-row source (a Nested Loop Left Join: the Result outer, the target's scan inner), UPDATE action
#  (b) the same source, DELETE action
#  (c) a table source hashed (nested loops and merge joins off), UPDATE action, one of its two rows deleted by s1
#  (d) s1 aborts: the UPDATE action updates the original row through the trigger (control)
#  (e) the target hashed on the join's nullable side (a three-row target, a fifty-row source probing it), UPDATE action
# Expected output is PostgreSQL 18.6's (the C oracle).

setup
{
 CREATE TABLE dt (k int PRIMARY KEY, v int, note text);
 CREATE FUNCTION dt_br() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF TG_OP = 'DELETE' THEN RETURN OLD; END IF; NEW.note := 'br'; RETURN NEW; END $$;
 CREATE TRIGGER dt_br BEFORE UPDATE OR DELETE ON dt FOR EACH ROW EXECUTE FUNCTION dt_br();
 CREATE TABLE ds (k int, v int);
 INSERT INTO dt SELECT g, g, 'i' FROM generate_series(1, 20) g;
 INSERT INTO ds VALUES (3, 30), (4, 40);
 CREATE TABLE dt2 (k int PRIMARY KEY, v int, note text);
 CREATE TRIGGER dt2_br BEFORE UPDATE OR DELETE ON dt2 FOR EACH ROW EXECUTE FUNCTION dt_br();
 CREATE TABLE ds2 (k int, v int);
 INSERT INTO dt2 VALUES (3, 3, 'i'), (4, 4, 'i'), (5, 5, 'i');
 INSERT INTO ds2 SELECT g, g * 10 FROM generate_series(1, 50) g;
 ANALYZE dt, ds, dt2, ds2;
}

teardown
{
 DROP TABLE dt, ds, dt2, ds2;
 DROP FUNCTION dt_br();
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1del3	{ DELETE FROM dt WHERE k = 3; }
step s1del5	{ DELETE FROM dt WHERE k = 5; }
step s1del4b	{ DELETE FROM dt2 WHERE k = 4; }
step s1c	{ COMMIT; }
step s1a	{ ROLLBACK; }

session s2
step s2upd	{ MERGE INTO dt t USING (SELECT 5 AS k, 50 AS v) s ON t.k = s.k WHEN MATCHED THEN UPDATE SET v = s.v WHEN NOT MATCHED THEN INSERT VALUES (s.k, s.v + 100, 'nm'); }
step s2del	{ MERGE INTO dt t USING (SELECT 5 AS k, 50 AS v) s ON t.k = s.k WHEN MATCHED THEN DELETE WHEN NOT MATCHED THEN INSERT VALUES (s.k, s.v + 200, 'nm'); }
step s2sel	{ SELECT * FROM dt WHERE k IN (3, 4, 5) ORDER BY k; }
step s2selb	{ SELECT * FROM dt2 WHERE k IN (3, 4, 5, 6) ORDER BY k; }

session s3
setup		{ SET enable_nestloop = off; SET enable_mergejoin = off; }
step s3upd	{ MERGE INTO dt t USING ds s ON t.k = s.k WHEN MATCHED THEN UPDATE SET v = s.v WHEN NOT MATCHED THEN INSERT VALUES (s.k, s.v + 100, 'nm'); }
step s3updb	{ MERGE INTO dt2 t USING ds2 s ON t.k = s.k WHEN MATCHED THEN UPDATE SET v = s.v WHEN NOT MATCHED THEN INSERT VALUES (s.k, s.v + 100, 'nm'); }

permutation s1del5 s2upd s1c s2sel
permutation s1del5 s2del s1c s2sel
permutation s1del3 s3upd s1c s2sel
permutation s1del5 s2upd s1a s2sel
permutation s1del4b s3updb s1c s2selb

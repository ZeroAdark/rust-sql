# semantics D2673 (the D2028 residual): a write's EvalPlanQual beside an InitPlan attached inside a correlated SubPlan's plan,
# whose one-time filter calls a VOLATILE function. PostgreSQL 18.6's EvalPlanQualStart initializes every SubPlan's plan
# afresh in the recheck's EState (ExecInitSubPlan arms each InitPlan inside one), so the first recheck that reads such an
# InitPlan evaluates it again, and the statement's run keeps its own value; a later recheck reads the value
# EvalPlanQualBegin copies in from the statement's run.
# u1 / s1 / d1 (the InitPlan reads nothing outside it: one call in the statement's run, one at the recheck): nextval 2.
#   u1 / d1 read it in the SubPlan's childless Result, s1 in its index-less scan's qual; d1 is a DELETE .. RETURNING, served
#   through the entry seam.
# j1 (the same over a join the recheck rebuilds first): nextval 2.
# t1 (two rows rechecked): nextval 2; t2 (the InitPlan's value is the call's: nextval in SET): the first recheck adds the
#   value 2, the second the statement run's 1 (id 2 -> 14, id 3 -> 14).
# c1 (the InitPlan reads the outer row: the SubPlan's rescan re-arms it at every call, the recheck's too): nextval 4.
setup
{
 CREATE TABLE w7q (id int PRIMARY KEY, c int);
 INSERT INTO w7q SELECT i, i FROM generate_series(1, 8) i;
 CREATE TABLE w7k (k int);
 INSERT INTO w7k SELECT i FROM generate_series(1, 12) i;
 CREATE TABLE w7j (id int, z int);
 INSERT INTO w7j SELECT i, i FROM generate_series(1, 8) i;
 CREATE SEQUENCE w7_seq;
 CREATE FUNCTION w7_bump(int) RETURNS boolean LANGUAGE plpgsql VOLATILE AS $$ BEGIN PERFORM nextval('w7_seq'); RETURN $1 > 0; END $$;
 CREATE FUNCTION w7_next() RETURNS int LANGUAGE sql VOLATILE AS $$ SELECT nextval('w7_seq')::int $$;
 ANALYZE w7q, w7k, w7j;
}

teardown
{
 DROP TABLE w7q, w7k, w7j;
 DROP FUNCTION w7_bump(int);
 DROP FUNCTION w7_next();
 DROP SEQUENCE w7_seq;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step b3		{ UPDATE w7q SET c = c + 10 WHERE id = 3; }
step b23	{ UPDATE w7q SET c = c + 10 WHERE id IN (2, 3); }
step bc		{ COMMIT; }

session s2
step u1		{ UPDATE w7q SET c = c + 1 WHERE id <= 3 AND c < (SELECT w7q.id + 100 + (SELECT 0 WHERE w7_bump(1))); }
step s1		{ UPDATE w7q SET c = c + 1 WHERE id <= 3 AND EXISTS (SELECT 1 FROM w7k WHERE k = w7q.id AND k < (SELECT 9 WHERE w7_bump(1))); }
step d1		{ DELETE FROM w7q WHERE id <= 3 AND EXISTS (SELECT 1 FROM w7k WHERE k = w7q.id AND k < (SELECT 9 WHERE w7_bump(1))) RETURNING id; }
step j1		{ UPDATE w7q SET c = c + 1 FROM w7j WHERE w7q.id = w7j.id AND w7q.id <= 3 AND w7q.c < (SELECT w7q.id + 100 + (SELECT 0 WHERE w7_bump(1))); }
step t1		{ UPDATE w7q SET c = c + 1 WHERE id <= 3 AND c < (SELECT w7q.id + 100 + (SELECT 0 WHERE w7_bump(1))) RETURNING id, c; }
step t2		{ UPDATE w7q SET c = c + (SELECT w7q.id * 0 + (SELECT w7_next())) WHERE id <= 3; }
step c1		{ UPDATE w7q SET c = c + 1 WHERE id <= 3 AND c < (SELECT (SELECT w7q.id + 100 WHERE w7_bump(w7q.id))); }
step chk	{ SELECT id, c FROM w7q ORDER BY id; }
step seq	{ SELECT last_value, is_called FROM w7_seq; }

permutation b3 u1 bc chk seq
permutation b3 s1 bc chk seq
permutation b3 d1 bc chk seq
permutation b3 j1 bc chk seq
permutation b23 t1 bc chk seq
permutation b23 t2 bc chk seq
permutation b3 c1 bc chk seq

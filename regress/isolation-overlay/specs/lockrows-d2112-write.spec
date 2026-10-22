# lockrows D2112 (the write's half): PG 18.6's EvalPlanQualBegin forces every pending InitPlan in the ModifyTable
# subplan's extParam at the recheck (ExecSetParamPlanMulti), even one the recheck never reads. UPDATE / DELETE whose
# InitPlan (nextval) is read only in an untaken CASE arm before the write: PG's recheck advances the sequence once.
# Written by lockrows; semantics' D2112 phase B fix serves it: the runner's write recheck forces each pending Init the
# subplan's extParam holds (decompose recheck_inits, Init proto INIT_RECHECK, exec rerun/epq.rs recheck_leader) and reuses
# an evaluated one (its members recheck_skip). The unconditional read (s2ux, a SET entry) is the control.
setup
{
 CREATE TABLE wi (id int PRIMARY KEY, grp int, v int);
 INSERT INTO wi SELECT i, i % 3, i FROM generate_series(1, 12) i;
 CREATE SEQUENCE wisq;
}
teardown
{
 DROP SEQUENCE wisq;
 DROP TABLE wi;
}
session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1v	{ UPDATE wi SET v = v + 100 WHERE id = 3; }
step s1c	{ COMMIT; }
session s2
step s2b	{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s2ur	{ UPDATE wi SET v = CASE WHEN v > 0 THEN v + 1 ELSE (SELECT nextval('wisq'))::int END WHERE grp = 0 RETURNING id, v; }
step s2dr	{ DELETE FROM wi WHERE grp = 0 AND CASE WHEN v > 0 THEN true ELSE (SELECT nextval('wisq') > 0) END RETURNING id; }
step s2ux	{ UPDATE wi SET v = v + (SELECT nextval('wisq'))::int WHERE grp = 0 RETURNING id, v; }
step s2seq	{ SELECT last_value, is_called FROM wisq; }
step s2c	{ COMMIT; }
permutation s2b s1v s2ur s1c s2seq s2c
permutation s2b s1v s2dr s1c s2seq s2c
permutation s2b s1v s2ux s1c s2seq s2c

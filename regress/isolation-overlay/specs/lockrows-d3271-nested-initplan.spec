# lockrows D3271 (rest): InitPlans at the EvalPlanQual recheck of a row lock that is not the top plan's own: a lock inside a
# FROM subquery (under a SubqueryScan that projects) and a lock inside a CTE, the statement's one lock. PostgreSQL 18.6's
# EvalPlanQualStart forces each pending InitPlan attached at the lock or above it whose param the lock's subplan reads
# (ExecSetParamPlanMulti over its extParam), copies every param value, then initializes every subplan of the statement in
# the recheck's estate: an InitPlan attached inside a CTE's plan (the CTE's lock's own included) is re-armed there, so a
# recheck that reads it runs it again. A LIMIT count's InitPlan above the lock is never read by the recheck. The sequence
# counts the InitPlan calls. A re-armed InitPlan that reads another CTE (eval-plan-qual's readwcte form) reads the
# statement's CTE store again (the copied CTE param names the store's leader): the CTE's own plan never runs again.
# Expected output is the C oracle's (PostgreSQL 18, pgdg).
setup
{
  CREATE TABLE wi (id int PRIMARY KEY, grp int NOT NULL, v int NOT NULL);
  INSERT INTO wi SELECT i, i % 3, i FROM generate_series(1, 12) i;
  CREATE SEQUENCE wisq;
  ANALYZE wi;
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
step s2sq	{ SELECT v + 1 AS w FROM (SELECT id, v FROM wi WHERE grp = 0 AND CASE WHEN v > 0 THEN true ELSE (SELECT nextval('wisq') > 0) END FOR UPDATE) s ORDER BY w; }
step s2squ	{ SELECT v + 1 AS w FROM (SELECT id, v FROM wi WHERE grp = 0 AND v > (SELECT nextval('wisq'))::int - 1000 FOR UPDATE) s ORDER BY w; }
step s2cu	{ WITH c AS (SELECT id, v FROM wi WHERE grp = 0 AND v > (SELECT nextval('wisq'))::int - 1000 FOR UPDATE) SELECT * FROM c ORDER BY id; }
step s2cc	{ WITH c1 AS (SELECT nextval('wisq')::int AS n), c2 AS (SELECT id, v FROM wi WHERE grp = 0 AND v > (SELECT n FROM c1) - 1000 FOR UPDATE) SELECT * FROM c2 ORDER BY id; }
step s2ca	{ WITH c AS (SELECT id, v FROM wi WHERE grp = 0 AND CASE WHEN v > 0 THEN true ELSE (SELECT nextval('wisq') > 0) END FOR UPDATE) SELECT count(*), sum(v) FROM c; }
step s2lim	{ SELECT id, v FROM wi WHERE grp = 0 ORDER BY id LIMIT (SELECT nextval('wisq'))::int + 2 FOR UPDATE; }
step s2seq	{ SELECT last_value, is_called FROM wisq; }
step s2c	{ COMMIT; }

permutation s2b s1v s2sq s1c s2seq s2c
permutation s2b s1v s2squ s1c s2seq s2c
permutation s2b s1v s2cu s1c s2seq s2c
permutation s2b s1v s2cc s1c s2seq s2c
permutation s2b s1v s2ca s1c s2seq s2c
permutation s2b s1v s2lim s1c s2seq s2c

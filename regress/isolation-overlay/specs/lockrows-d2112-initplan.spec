# lockrows D2112: InitPlans at a row lock's EvalPlanQual recheck. PostgreSQL 18.6's EvalPlanQualStart forces every
# pending InitPlan in the rechecked subplan's extParam (ExecSetParamPlanMulti), even one the recheck never evaluates, and
# initializes every subplan of the statement in the recheck's estate (ExecInitSubPlan re-arms each InitPlan attached
# inside one, so the first recheck reads it again). Forms: a lock over its own scan with an InitPlan read only in an
# untaken CASE arm (pending at the recheck), the same under a LIMIT (over a sort, and not) and in a product (an aggregate
# over a FOR UPDATE subquery; and that product reading it at its first row), over a join, the CTE form pending, and the
# control (an InitPlan read at the first row: no extra call). A CTE's lock whose scan reads an InitPlan (PG re-arms it, a
# second call) is in lockrows-d3271-nested-initplan.
# Expected output is the C oracle's (PostgreSQL 18, pgdg).
setup
{
  CREATE TABLE wi (id int PRIMARY KEY, grp int NOT NULL, v int NOT NULL);
  INSERT INTO wi SELECT i, i % 3, i FROM generate_series(1, 12) i;
  CREATE TABLE wj (id int PRIMARY KEY, w int NOT NULL);
  INSERT INTO wj SELECT i, i * 10 FROM generate_series(0, 2) i;
  CREATE SEQUENCE wisq;
  ANALYZE wi, wj;
}

teardown
{
  DROP SEQUENCE wisq;
  DROP TABLE wi, wj;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1v	{ UPDATE wi SET v = v + 100 WHERE id = 3; }
step s1c	{ COMMIT; }

session s2
step s2b	{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s2l	{ SELECT id, v FROM wi WHERE grp = 0 AND CASE WHEN v > 0 THEN true ELSE (SELECT nextval('wisq') > 0) END FOR UPDATE; }
step s2lim	{ SELECT id, v FROM wi WHERE grp = 0 AND CASE WHEN v > 0 THEN true ELSE (SELECT nextval('wisq') > 0) END ORDER BY id LIMIT 2 FOR UPDATE; }
step s2lim2	{ SELECT id, v FROM wi WHERE grp = 0 AND CASE WHEN v > 0 THEN true ELSE (SELECT nextval('wisq') > 0) END LIMIT 2 FOR UPDATE; }
step s2p	{ SELECT count(*), sum(v) FROM (SELECT v FROM wi WHERE grp = 0 AND CASE WHEN v > 0 THEN true ELSE (SELECT nextval('wisq') > 0) END FOR UPDATE) s; }
step s2pu	{ SELECT count(*), sum(v) FROM (SELECT v FROM wi WHERE grp = 0 AND v > (SELECT nextval('wisq'))::int - 1000 FOR UPDATE) s; }
step s2j	{ SELECT a.id, a.v, b.w FROM wi a JOIN wj b ON b.id = a.grp WHERE a.grp = 0 AND CASE WHEN a.v > 0 THEN true ELSE (SELECT nextval('wisq') > 0) END FOR UPDATE OF a; }
step s2ctp	{ WITH c AS (SELECT id, v FROM wi WHERE grp = 0 AND CASE WHEN v > 0 THEN true ELSE (SELECT nextval('wisq') > 0) END FOR UPDATE) SELECT * FROM c ORDER BY id; }
step s2u	{ SELECT id, v FROM wi WHERE grp = 0 AND v > (SELECT nextval('wisq'))::int - 1000 FOR UPDATE; }
step s2seq	{ SELECT last_value, is_called FROM wisq; }
step s2c	{ COMMIT; }

permutation s2b s1v s2l s1c s2seq s2c
permutation s2b s1v s2lim s1c s2seq s2c
permutation s2b s1v s2lim2 s1c s2seq s2c
permutation s2b s1v s2p s1c s2seq s2c
permutation s2b s1v s2pu s1c s2seq s2c
permutation s2b s1v s2j s1c s2seq s2c
permutation s2b s1v s2ctp s1c s2seq s2c
permutation s2b s1v s2u s1c s2seq s2c

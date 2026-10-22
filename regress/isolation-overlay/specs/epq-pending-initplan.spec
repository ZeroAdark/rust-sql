# EvalPlanQual forces every pending initPlan the recheck plan references
# (ExecSetParamPlanMulti(extParam) in EvalPlanQualStart/Begin, execMain.c
# 3076/2972): the never-taken division-by-zero arm still fails the UPDATE
# after a concurrent update, and the row keeps the first writer's value.
# Expected output is the C oracle's (PostgreSQL 18.6).

setup
{
 CREATE TABLE pit (k int PRIMARY KEY, v int);
 CREATE TABLE piu (k int);
 INSERT INTO pit VALUES (1, 10);
}

teardown
{
 DROP TABLE pit;
 DROP TABLE piu;
}

session s1
step s1b	{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1u	{ UPDATE pit SET v = v + 1 WHERE k = 1; }
step s1c	{ COMMIT; }

session s2
step expl	{ EXPLAIN (COSTS OFF) UPDATE pit SET v = CASE WHEN v IS NULL THEN (SELECT 1 / count(*) FROM piu WHERE k < 0) ELSE v + 1 END WHERE k = 1; }
step upd	{ UPDATE pit SET v = CASE WHEN v IS NULL THEN (SELECT 1 / count(*) FROM piu WHERE k < 0) ELSE v + 1 END WHERE k = 1 RETURNING v; }

session s3
step sel	{ SELECT k, v FROM pit ORDER BY k; }

# Uncontended: the initPlan is never demanded, the ELSE arm wins.
permutation expl upd sel
# Contended: the recheck forces the initPlan first -> 22012, row keeps 11.
permutation expl s1b s1u upd s1c sel

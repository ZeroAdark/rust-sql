# D2052: an EvalPlanQual recheck forces the pending InitPlans its plan references (C EvalPlanQualStart /
# EvalPlanQualBegin: ExecSetParamPlanMulti(epqstate->plan->extParam)). The UPDATE's InitPlan is behind a CASE arm the
# row never takes, so the first pass never runs it; the recheck after s1's commit forces it, and its division by zero
# raises, as in PostgreSQL 18.6. A recheck whose InitPlan does not raise updates from the new row version. Gravity
# serves this UPDATE too, and its own EPQ recheck raises there as well (arms a and b, W0 and W8), so the spec runs in
# schedule.extra.
setup
{
 CREATE TABLE ip_t (id int PRIMARY KEY, v int);
 INSERT INTO ip_t VALUES (1, 1), (2, 2);
}
teardown { DROP TABLE ip_t; }
session s1
setup { BEGIN ISOLATION LEVEL READ COMMITTED; }
step u1 { UPDATE ip_t SET v = 5 WHERE id = 1; }
step c1 { COMMIT; }
session s2
setup { BEGIN ISOLATION LEVEL READ COMMITTED; }
step raise { UPDATE ip_t SET v = CASE WHEN v > 100 THEN (SELECT 1 / (count(*) - count(*))::int FROM pg_class) ELSE v + 1 END WHERE id = 1 RETURNING v; }
step ok { UPDATE ip_t SET v = CASE WHEN v > 100 THEN (SELECT count(*)::int FROM pg_class WHERE false) ELSE v + 1 END WHERE id = 1 RETURNING v; }
step c2 { COMMIT; }
session s3
step read { SELECT * FROM ip_t ORDER BY id; }
permutation u1 raise c1 c2 read
permutation u1 ok c1 c2 read
permutation raise u1 c2 c1 read

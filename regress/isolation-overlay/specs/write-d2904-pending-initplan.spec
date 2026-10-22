# D2904: a write's EvalPlanQual beside an InitPlan its filter reads only in an untaken CASE arm. PostgreSQL's
# EvalPlanQualStart forces each pending InitPlan attached at the ModifyTable or above that the recheck's plan reads
# (ExecSetParamPlanMulti over its extParam) before the redo: when s1 changed a target row (READ COMMITTED), the forced
# InitPlan raises (division by zero) or makes its VOLATILE call there, though the main run never evaluated it. Forms: an
# UPDATE and a DELETE whose filter also holds a per-row scalar SubPlan step (D652-S1's recheck in the IR, D652-I), two
# InitPlans in one CASE (the taken arm's evaluated, the untaken arm's pending), a VOLATILE pending one (one call at the
# recheck), the same write with no SubPlan step (the runner's recheck), and an InitPlan the filter reads unconditionally
# (evaluated before any row reaches the write: its value copied, no error). In SET: an InitPlan read unconditionally in
# the new value (the subplan's projection evaluates it for every row before the write: copied, kept in the IR) and one
# read only in an untaken CASE arm of the new value (pending: forced at the recheck). Expected output is the C oracle's
# (PostgreSQL 18, pgdg).
setup
{
  CREATE TABLE wjob (id int PRIMARY KEY, prio int NOT NULL, attempts int NOT NULL DEFAULT 0);
  INSERT INTO wjob SELECT i, CASE WHEN i % 2 = 1 THEN 9 ELSE 1 END, 0 FROM generate_series(1, 10) i;
  CREATE TABLE wcfg (name text PRIMARY KEY, lim int NOT NULL);
  INSERT INTO wcfg VALUES ('q', 8);
  CREATE SEQUENCE wsq;
  ANALYZE wjob, wcfg;
}
teardown { DROP TABLE wjob, wcfg; DROP SEQUENCE wsq; }

session s1
setup		{ BEGIN; }
step s1a	{ UPDATE wjob SET attempts = attempts + 100 WHERE id = 5; }
step s1q	{ UPDATE wcfg SET lim = 0 WHERE name = 'q'; }
step s1c	{ COMMIT; }
teardown	{ ROLLBACK; }

session s2
setup		{ SET enable_seqscan = off; SET enable_bitmapscan = off; }
step s2x	{ EXPLAIN (COSTS OFF) UPDATE wjob j SET attempts = j.attempts + 1 WHERE j.id <= 8 AND CASE WHEN j.prio > 100 THEN j.attempts > (SELECT 1 / (lim - lim) FROM wcfg WHERE name = 'q') ELSE true END AND j.prio = (SELECT k.prio FROM wjob k WHERE k.id = j.id) RETURNING j.id, j.attempts; }
step s2u	{ UPDATE wjob j SET attempts = j.attempts + 1 WHERE j.id <= 8 AND CASE WHEN j.prio > 100 THEN j.attempts > (SELECT 1 / (lim - lim) FROM wcfg WHERE name = 'q') ELSE true END AND j.prio = (SELECT k.prio FROM wjob k WHERE k.id = j.id) RETURNING j.id, j.attempts; }
step s2d	{ DELETE FROM wjob j WHERE j.id <= 8 AND CASE WHEN j.prio > 100 THEN j.attempts > (SELECT 1 / (lim - lim) FROM wcfg WHERE name = 'q') ELSE true END AND j.prio = (SELECT k.prio FROM wjob k WHERE k.id = j.id) RETURNING j.id; }
step s2t	{ UPDATE wjob j SET attempts = j.attempts + 1 WHERE j.id <= 8 AND CASE WHEN j.prio > 0 THEN j.attempts >= (SELECT 0 * lim FROM wcfg WHERE name = 'q') ELSE j.attempts > (SELECT 1 / (lim - lim) FROM wcfg WHERE name = 'q') END AND j.prio = (SELECT k.prio FROM wjob k WHERE k.id = j.id) RETURNING j.id, j.attempts; }
step s2v	{ UPDATE wjob j SET attempts = j.attempts + 1 WHERE j.id <= 8 AND CASE WHEN j.prio > 100 THEN j.attempts > (SELECT nextval('wsq') * 0) ELSE true END AND j.prio = (SELECT k.prio FROM wjob k WHERE k.id = j.id) RETURNING j.id, j.attempts; }
step s2p	{ UPDATE wjob j SET attempts = j.attempts + 1 WHERE j.id <= 8 AND CASE WHEN j.prio > 100 THEN j.attempts > (SELECT 1 / (lim - lim) FROM wcfg WHERE name = 'q') ELSE true END RETURNING j.id, j.attempts; }
step s2r	{ UPDATE wjob j SET attempts = j.attempts + 1 WHERE j.attempts >= (SELECT 0 * lim FROM wcfg WHERE name = 'q') AND j.id <= (SELECT lim FROM wcfg WHERE name = 'q') AND j.prio = (SELECT k.prio FROM wjob k WHERE k.id = j.id) RETURNING j.id, j.attempts; }
step s2s	{ UPDATE wjob j SET attempts = j.attempts + 1 + (SELECT 0 * lim FROM wcfg WHERE name = 'q') WHERE j.id <= 8 AND j.prio = (SELECT k.prio FROM wjob k WHERE k.id = j.id) RETURNING j.id, j.attempts; }
step s2c	{ UPDATE wjob j SET attempts = CASE WHEN j.prio > 100 THEN (SELECT 1 / (lim - lim) FROM wcfg WHERE name = 'q') ELSE j.attempts + 1 END WHERE j.id <= 8 AND j.prio = (SELECT k.prio FROM wjob k WHERE k.id = j.id) RETURNING j.id, j.attempts; }
step s2seq	{ SELECT last_value, is_called FROM wsq; }

permutation s2x
permutation s1a s2u s1c
permutation s1a s2d s1c
permutation s1a s2t s1c
permutation s1a s2v s1c s2seq
permutation s1a s2p s1c
permutation s1q s1a s2r s1c
permutation s1a s2s s1c
permutation s1q s1a s2s s1c
permutation s1a s2c s1c

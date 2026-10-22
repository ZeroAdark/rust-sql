# writesubplan (regress merge#297, D1112 nest.subplan B4's MERGE RETURNING part): a MERGE whose RETURNING list holds
# SubPlans (a lookup keyed by merge_action(), one over OLD and NEW) while another session changes the matched rows
# (READ COMMITTED). PostgreSQL 18.6 evaluates the list per returned row after the action, over the action's OLD / NEW:
#  (a) a matched row concurrently deleted: ExecMergeMatched meets TM_Deleted and falls through to the NOT MATCHED
#      INSERT, whose row returns merge_action() 'INSERT' (the lookup reads 'ins', OLD is NULL)
#  (b) a matched row concurrently updated: the EvalPlanQual recheck takes the newest version, so OLD is s1's version
#      and NEW the action's row over it (the target hashed, then a one-row source: a Nested Loop)
#  (c) s1 aborts: the first version's UPDATE returns (control)
# Expected output is PostgreSQL 18.6's (the C oracle).

setup
{
 CREATE TABLE wmr (k int PRIMARY KEY, v int, note text);
 CREATE TABLE wms (k int, v int);
 CREATE TABLE wma (action text, abbrev text);
 INSERT INTO wmr VALUES (4, 4, 'i'), (5, 5, 'i'), (6, 6, 'i');
 INSERT INTO wms SELECT g, g * 10 FROM generate_series(4, 50) g;
 INSERT INTO wma VALUES ('INSERT', 'ins'), ('UPDATE', 'upd'), ('DELETE', 'del');
 ANALYZE wmr, wms, wma;
}

teardown
{
 DROP TABLE wmr, wms, wma;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1del4	{ DELETE FROM wmr WHERE k = 4; }
step s1upd5	{ UPDATE wmr SET v = 500 WHERE k = 5; }
step s1c	{ COMMIT; }
step s1a	{ ROLLBACK; }

session s2
setup		{ SET enable_nestloop = off; SET enable_mergejoin = off; }
step s2hash	{ MERGE INTO wmr t USING wms s ON t.k = s.k WHEN MATCHED AND t.k IN (4, 5) THEN UPDATE SET v = t.v + s.v, note = 'u' WHEN NOT MATCHED AND s.k = 4 THEN INSERT VALUES (s.k, s.v + 1, 'nm') RETURNING (SELECT abbrev FROM wma WHERE action = merge_action()) AS act, old.v AS old_v, (SELECT new.v - old.v) AS dv, t.*; }
step s2sel	{ SELECT * FROM wmr ORDER BY k; }

session s3
step s3nest	{ MERGE INTO wmr t USING (SELECT 5 AS k, 50 AS v) s ON t.k = s.k WHEN MATCHED THEN UPDATE SET v = t.v + s.v, note = 'u' WHEN NOT MATCHED THEN INSERT VALUES (s.k, s.v + 1, 'nm') RETURNING (SELECT abbrev FROM wma WHERE action = merge_action()) AS act, (SELECT old.v) AS old_v, new.v; }

permutation s1del4 s2hash s1c s2sel
permutation s1upd5 s2hash s1c s2sel
permutation s1upd5 s3nest s1c s2sel
permutation s1del4 s2hash s1a s2sel

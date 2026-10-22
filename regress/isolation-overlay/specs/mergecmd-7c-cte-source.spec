# mergecmd 7c (with#245's form): a MERGE whose source is a MATERIALIZED CTE pulled up into the join (its scan's row a
# COPY mark) while the actions read InitPlans over the same CTE, and another session changes the matched row (READ
# COMMITTED). PostgreSQL 18.6's ExecMergeMatched rechecks the newest version under EvalPlanQual with the source row
# handed over whole (the COPY mark: the CTE is never read again); an action's InitPlan is evaluated once, lazily, in the
# main estate over the CTE's spool. The target has 2000 rows and a primary key: a Nested Loop Left Join over its index
# scan, the CTE scan outer (s2exp_mn pins the plan; with the target hashed instead, the drain under the write refuses
# by its own name).
#  (a) s1 updates the matched row: the UPDATE reads the newest version, its SET the InitPlan over the CTE
#  (b) s1 moves the row's key away: the recheck finds no match, the NOT MATCHED INSERT reads the other InitPlan (first
#      read on the recheck)
#  (c) s1 deletes the row: the NOT MATCHED INSERT runs
#  (d) the SET also reads the source row's columns: they are the stored row's
#  (e) s1 aborts: the original version is updated (control)
# Expected output is PostgreSQL 18.6's (the C oracle).
setup
{
 CREATE TABLE mn (k int PRIMARY KEY, v text);
 INSERT INTO mn SELECT i, i || ' v' FROM generate_series(1, 2000) i;
 ANALYZE mn;
}

teardown
{
 DROP TABLE mn;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1upd_mn	{ UPDATE mn SET v = v || ' s1' WHERE k = 13; }
step s1key_mn	{ UPDATE mn SET k = 1300 WHERE k = 13; }
step s1del_mn	{ DELETE FROM mn WHERE k = 13; }
step s1c	{ COMMIT; }
step s1a	{ ROLLBACK; }

session s2
step s2exp_mn	{ EXPLAIN (COSTS OFF) WITH merge_source_cte AS MATERIALIZED (SELECT 13 a, 'src' b) MERGE INTO mn USING (SELECT * FROM merge_source_cte) o ON mn.k = o.a WHEN MATCHED THEN UPDATE SET v = (SELECT b || merge_source_cte.*::text || ' merge update' FROM merge_source_cte WHERE a = 13) WHEN NOT MATCHED THEN INSERT VALUES (o.a, o.b || (SELECT merge_source_cte.*::text || ' merge insert' FROM merge_source_cte)); }
step s2cte_mn	{ WITH merge_source_cte AS MATERIALIZED (SELECT 13 a, 'src' b) MERGE INTO mn USING (SELECT * FROM merge_source_cte) o ON mn.k = o.a WHEN MATCHED THEN UPDATE SET v = (SELECT b || merge_source_cte.*::text || ' merge update' FROM merge_source_cte WHERE a = 13) WHEN NOT MATCHED THEN INSERT VALUES (o.a, o.b || (SELECT merge_source_cte.*::text || ' merge insert' FROM merge_source_cte)); }
step s2src_mn	{ WITH merge_source_cte AS MATERIALIZED (SELECT 13 a, 'src' b) MERGE INTO mn USING (SELECT * FROM merge_source_cte) o ON mn.k = o.a WHEN MATCHED THEN UPDATE SET v = mn.v || ' + ' || o.b || (SELECT merge_source_cte.*::text FROM merge_source_cte WHERE a = 13) WHEN NOT MATCHED THEN INSERT VALUES (o.a + 1000, o.b); }
step s2sel_mn	{ SELECT * FROM mn WHERE k IN (13, 1013, 1300) ORDER BY k; }

permutation s2exp_mn s1upd_mn s2cte_mn s1c s2sel_mn
permutation s1key_mn s2cte_mn s1c s2sel_mn
permutation s1del_mn s2cte_mn s1c s2sel_mn
permutation s1upd_mn s2src_mn s1c s2sel_mn
permutation s1upd_mn s2cte_mn s1a s2sel_mn

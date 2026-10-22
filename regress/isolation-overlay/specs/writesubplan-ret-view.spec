# writesubplan (regress updatable_views#108 / #111 / #139, D1112 RETURNING over a view): a MERGE into an auto-updatable
# view whose RETURNING list reads the view's SubPlan column through OLD / NEW (PG 18's ReturningExpr) or an explicit
# (SELECT old), over a Hash Right Join (the base table probing, the VALUES hashed), while another session changes the
# matched rows (READ COMMITTED). PostgreSQL 18.6 evaluates the list per returned row after the action, over the
# action's OLD / NEW, an image that does not exist reading NULL without running its SubPlan:
#  (a) a matched row concurrently updated: the EvalPlanQual recheck takes the newest version (OLD = s1's version)
#  (b) a matched row concurrently deleted: ExecMergeMatched meets TM_Deleted and falls through to the NOT MATCHED
#      INSERT (OLD NULL, its e column's division never runs)
#  (c) s1 aborts: the first versions return (control)
# Expected output is PostgreSQL 18.6's (the C oracle).

setup
{
 CREATE TABLE wrb (a int PRIMARY KEY, b text DEFAULT 'u');
 INSERT INTO wrb SELECT g, 'r' || g FROM generate_series(1, 60) g;
 CREATE VIEW wrbv AS SELECT *, (SELECT concat('b: ', b)) AS d, (SELECT 10 / coalesce(a, 0)) AS e FROM wrb WHERE a > 0;
 CREATE VIEW wrbv1 AS SELECT b AS bb, a AS aa FROM wrb WHERE a > 0;
 CREATE VIEW wrbv2 AS SELECT aa AS aaa, bb AS bbb, 'K' AS k FROM wrbv1 WHERE aa < 100;
 ANALYZE wrb;
}

teardown
{
 DROP VIEW wrbv2, wrbv1, wrbv;
 DROP TABLE wrb;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1upd2	{ UPDATE wrb SET b = 's1' WHERE a = 2; }
step s1del3	{ DELETE FROM wrb WHERE a = 3; }
step s1c	{ COMMIT; }
step s1a	{ ROLLBACK; }

session s2
setup		{ SET enable_nestloop = off; SET enable_mergejoin = off; }
step s2merge	{ MERGE INTO wrbv t USING (VALUES (2, 'm2'), (3, 'm3'), (70, 'm70')) AS v(a, b) ON t.a = v.a WHEN MATCHED AND t.a = 2 THEN UPDATE SET b = v.b WHEN MATCHED THEN DELETE WHEN NOT MATCHED AND v.a > 0 THEN INSERT (a, b) VALUES (v.a, v.b) RETURNING merge_action(), v.a, old.d, new.d, old.e, new.e, t.d; }
step s2sel	{ SELECT * FROM wrbv WHERE a IN (2, 3, 70) ORDER BY a; }

session s3
setup		{ SET enable_nestloop = off; SET enable_mergejoin = off; }
step s3merge	{ MERGE INTO wrbv2 t USING (VALUES (2, 'n2'), (3, 'n3'), (80, 'n80')) AS v(a, b) ON aaa = v.a WHEN MATCHED AND aaa = 3 THEN DELETE WHEN MATCHED THEN UPDATE SET bbb = v.b WHEN NOT MATCHED THEN INSERT (aaa) VALUES (v.a) RETURNING merge_action(), v.a, (SELECT old), (SELECT (SELECT new)); }

permutation s1upd2 s2merge s1c s2sel
permutation s1del3 s2merge s1c s2sel
permutation s1upd2 s2merge s1a s2sel
permutation s1upd2 s3merge s1c s2sel
permutation s1del3 s3merge s1c s2sel

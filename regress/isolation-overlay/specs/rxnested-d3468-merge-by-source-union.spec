# D3468: MERGE with WHEN NOT MATCHED BY SOURCE actions over a flattened UNION ALL whose members are bare relations (the
# planner pulls each relation itself into the appendrel), hashed under the join with the target probing (nested loops
# and merge joins off), while another session changes the target row (READ COMMITTED). The target is the join's outer
# side. PostgreSQL 18.6's ExecMergeMatched rechecks a MATCHED row by EvalPlanQual and then evaluates the join condition
# over the newest version and the ORIGINAL source row (its inner tuple stays the plan row): still true, the MATCHED
# action with that row; false, the NOT MATCHED BY SOURCE list over the newest version. A NOT MATCHED BY SOURCE row is
# refetched at its newest version and its action quals evaluated again; it never becomes MATCHED.
#  (a) a MATCHED row re-keyed onto another union row's key: NOT MATCHED BY SOURCE (a re-join would take that row)
#  (b) a MATCHED row updated in place: MATCHED, over the original source row
#  (c) a NOT MATCHED BY SOURCE row updated in place, and re-keyed past its action's qual
#  (d) NOT MATCHED BY SOURCE actions alone, over a re-keyed row
# The EXPLAIN step pins the plan. Expected output is PostgreSQL 18.6's (the C oracle).
setup
{
 CREATE TABLE um_t (id int PRIMARY KEY, hits int NOT NULL DEFAULT 0, v int NOT NULL DEFAULT 0);
 INSERT INTO um_t SELECT i, 0, 0 FROM generate_series(1, 2000) i WHERE i % 5 <> 0;
 CREATE TABLE um_a (k int, w int);
 INSERT INTO um_a VALUES (3, 1), (6, 2);
 CREATE TABLE um_b (k int, w int);
 INSERT INTO um_b VALUES (4, 101), (10, 103);
 ANALYZE um_t, um_a, um_b;
}

teardown
{
 DROP TABLE um_t, um_a, um_b;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1key	{ UPDATE um_t SET id = 10 WHERE id = 3; }
step s1v	{ UPDATE um_t SET v = 7 WHERE id = 3; }
step s1nv	{ UPDATE um_t SET v = 8 WHERE id = 2; }
step s1nkey	{ UPDATE um_t SET id = 15 WHERE id = 2; }
step s1c	{ COMMIT; }

session s2
setup		{ SET enable_nestloop = off; SET enable_mergejoin = off; }
step s2exp	{ EXPLAIN (COSTS OFF) MERGE INTO um_t t USING (SELECT k, w FROM um_a UNION ALL SELECT k, w FROM um_b) s ON t.id = s.k WHEN MATCHED THEN UPDATE SET hits = t.hits + s.w WHEN NOT MATCHED BY SOURCE AND t.id <= 12 THEN UPDATE SET v = -1; }
step s2m	{ MERGE INTO um_t t USING (SELECT k, w FROM um_a UNION ALL SELECT k, w FROM um_b) s ON t.id = s.k WHEN MATCHED THEN UPDATE SET hits = t.hits + s.w WHEN NOT MATCHED BY SOURCE AND t.id <= 12 THEN UPDATE SET v = -1; }
step s2d	{ MERGE INTO um_t t USING (SELECT k, w FROM um_a UNION ALL SELECT k, w FROM um_b) s ON t.id = s.k WHEN NOT MATCHED BY SOURCE AND t.id <= 12 THEN DELETE; }
step s2sel	{ SELECT id, hits, v FROM um_t WHERE id <= 16 ORDER BY id; }

permutation s2exp s1key s2m s1c s2sel
permutation s1v s2m s1c s2sel
permutation s1nv s2m s1c s2sel
permutation s1nkey s2m s1c s2sel
permutation s1nkey s2d s1c s2sel
permutation s1key s2d s1c s2sel

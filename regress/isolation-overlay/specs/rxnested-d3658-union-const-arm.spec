# D3658: UPDATE / DELETE .. FROM, and a MERGE of WHEN MATCHED actions, over a flattened UNION ALL of a bare relation member
# and a constant arm, hashed under the join with the target probing (nested loops and merge joins off), while another
# session changes the target row (READ COMMITTED). PostgreSQL 18.6's EvalPlanQual scans every member of the union again,
# the constant arm's Result too, and joins them with the row's newest version: the UPDATE takes its values from the
# re-joined row (the constant one when the row was re-keyed onto its key), RETURNING and the MERGE's actions read the
# ORIGINAL source row.
#  (a) the matched row re-keyed onto the constant arm's key: UPDATE, UPDATE .. RETURNING, MERGE
#  (b) the row matched by the constant arm updated in place, and re-keyed off every key: DELETE
# The EXPLAIN step pins the plan. Expected output is PostgreSQL 18.6's (the C oracle).
setup
{
 CREATE TABLE uc_t (id int PRIMARY KEY, hits int NOT NULL DEFAULT 0);
 INSERT INTO uc_t SELECT i, 0 FROM generate_series(1, 2000) i WHERE i % 5 <> 0;
 CREATE TABLE uc_a (k int, w int);
 INSERT INTO uc_a VALUES (3, 1), (6, 2);
 ANALYZE uc_t, uc_a;
}

teardown
{
 DROP TABLE uc_t, uc_a;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1key	{ UPDATE uc_t SET id = 10 WHERE id = 3; }
step s1v	{ UPDATE uc_t SET hits = 5 WHERE id = 7; }
step s1off	{ UPDATE uc_t SET id = 15 WHERE id = 7; }
step s1c	{ COMMIT; }

session s2
setup		{ SET enable_nestloop = off; SET enable_mergejoin = off; }
step s2exp	{ EXPLAIN (COSTS OFF) UPDATE uc_t t SET hits = t.hits + s.w FROM (SELECT k, w FROM uc_a UNION ALL SELECT 10, 100) s WHERE t.id = s.k; }
step s2u	{ UPDATE uc_t t SET hits = t.hits + s.w FROM (SELECT k, w FROM uc_a UNION ALL SELECT 10, 100) s WHERE t.id = s.k; }
step s2ur	{ UPDATE uc_t t SET hits = t.hits + s.w FROM (SELECT k, w FROM uc_a UNION ALL SELECT 10, 100) s WHERE t.id = s.k RETURNING t.id, t.hits, s.k, s.w; }
step s2m	{ MERGE INTO uc_t t USING (SELECT k, w FROM uc_a UNION ALL SELECT 10, 100) s ON t.id = s.k WHEN MATCHED THEN UPDATE SET hits = t.hits + s.w; }
step s2d	{ DELETE FROM uc_t t USING (SELECT k, w FROM uc_a UNION ALL SELECT 7, 1) s WHERE t.id = s.k; }
step s2sel	{ SELECT id, hits FROM uc_t WHERE id <= 16 ORDER BY id; }

permutation s2exp s1key s2u s1c s2sel
permutation s1key s2ur s1c s2sel
permutation s1key s2m s1c s2sel
permutation s1v s2d s1c s2sel
permutation s1off s2d s1c s2sel

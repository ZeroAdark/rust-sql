# D3477: UPDATE / DELETE .. FROM, or MERGE with WHEN MATCHED actions only, over a flattened UNION ALL subquery (a
# scanned relation under a qual, a constant row; a bare relation member) whose result relation is hashed and the union
# probes it (nested loops and merge joins off), while another session changes the matched row (READ COMMITTED). The
# subquery's COPY mark names a relation no scan carries, so PostgreSQL 18.6's EvalPlanQual scans the union's members
# again, whole, and joins them with the row's newest version: an UPDATE takes its values from the first row joining it,
# a DELETE deletes it once, a MERGE calls the row MATCHED when a row joins it and acts with the ORIGINAL source row.
# The EXPLAIN steps pin the plans.
#  (a) s1 re-keys the matched row onto another union row's key: the UPDATE adds that row's w, the MERGE the original's
#  (b) s1 updates the matched row in place: the write runs over the newest version
#  (c) s1 deletes the matched row: no write
#  (d) s1 updates a union member's row: the write reads the rows it saw and does not wait
#  (e) the DELETE over the re-keyed row, and the bare-member union over it
#  (f) MERGE's WHEN qual over the original source row (DELETE, not the UPDATE the re-run row would pick), and two re-run
#      rows joining the newest version (the first acts, once)
# Expected output is PostgreSQL 18.6's (the C oracle).
setup
{
 CREATE TABLE ur_t (id int PRIMARY KEY, v int NOT NULL, hits int NOT NULL DEFAULT 0);
 INSERT INTO ur_t SELECT i, i * 10, 0 FROM generate_series(1, 20) i;
 CREATE TABLE ur_s (k int, w int);
 INSERT INTO ur_s SELECT 100 + i, i FROM generate_series(1, 3000) i;
 INSERT INTO ur_s VALUES (5, 50), (25, 250);
 ANALYZE ur_t, ur_s;
}

teardown
{
 DROP TABLE ur_t, ur_s;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1key	{ UPDATE ur_t SET id = 25 WHERE id = 5; }
step s1v	{ UPDATE ur_t SET v = v + 1 WHERE id = 5; }
step s1del	{ DELETE FROM ur_t WHERE id = 5; }
step s1src	{ UPDATE ur_s SET w = 999 WHERE k = 5; }
step s1c	{ COMMIT; }

session s2
setup		{ SET enable_nestloop = off; SET enable_mergejoin = off; }
step s2exp	{ EXPLAIN (COSTS OFF) UPDATE ur_t t SET hits = t.hits + s.w FROM (SELECT k, w FROM ur_s WHERE w > 0 UNION ALL SELECT 7, 1) s WHERE t.id = s.k; }
step s2u	{ UPDATE ur_t t SET hits = t.hits + s.w FROM (SELECT k, w FROM ur_s WHERE w > 0 UNION ALL SELECT 7, 1) s WHERE t.id = s.k; }
step s2d	{ DELETE FROM ur_t t USING (SELECT k, w FROM ur_s WHERE w > 0 UNION ALL SELECT 7, 1) s WHERE t.id = s.k AND s.w >= 1 RETURNING t.id, t.v; }
step s2ub	{ UPDATE ur_t t SET hits = t.hits + s.w FROM (SELECT k, w FROM ur_s UNION ALL SELECT 7, 1) s WHERE t.id = s.k; }
step s2mexp	{ EXPLAIN (COSTS OFF) MERGE INTO ur_t t USING (SELECT k, w FROM ur_s WHERE w > 0 UNION ALL SELECT 7, 1) s ON t.id = s.k WHEN MATCHED THEN UPDATE SET hits = t.hits + s.w; }
step s2m	{ MERGE INTO ur_t t USING (SELECT k, w FROM ur_s WHERE w > 0 UNION ALL SELECT 7, 1) s ON t.id = s.k WHEN MATCHED THEN UPDATE SET hits = t.hits + s.w; }
step s2mq	{ MERGE INTO ur_t t USING (SELECT k, w FROM ur_s WHERE w > 0 UNION ALL SELECT 7, 1) s ON t.id = s.k WHEN MATCHED AND s.w < 100 THEN DELETE WHEN MATCHED THEN UPDATE SET hits = 999; }
step s2m2	{ MERGE INTO ur_t t USING (SELECT k, w FROM ur_s WHERE w > 0 UNION ALL SELECT 25, 260) s ON t.id = s.k WHEN MATCHED THEN UPDATE SET hits = t.hits + s.w; }
step s2sel	{ SELECT id, v, hits FROM ur_t WHERE id IN (5, 7, 25) ORDER BY id; }

permutation s2exp s1key s2u s1c s2sel
permutation s1v s2u s1c s2sel
permutation s1del s2u s1c s2sel
permutation s1src s2u s1c s2sel
permutation s1key s2d s1c s2sel
permutation s1key s2ub s1c s2sel
permutation s2mexp s1key s2m s1c s2sel
permutation s1v s2m s1c s2sel
permutation s1del s2m s1c s2sel
permutation s1src s2m s1c s2sel
permutation s1key s2mq s1c s2sel
permutation s1key s2m2 s1c s2sel

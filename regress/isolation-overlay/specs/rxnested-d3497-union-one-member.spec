# D3497: UPDATE / DELETE .. FROM a flattened UNION ALL subquery whose pushed-down qual folds its constant arm false, so
# one member is live (a subquery under a WHERE, or a bare relation) and setrefs drops the one-child Append: no Append
# carries the subquery's COPY mark. The result relation is hashed and the member probes it (nested loops and merge joins
# off), while another session changes the matched row (READ COMMITTED). PostgreSQL 18.6's EvalPlanQual hands the mark to
# no scan and scans the member again, whole, joined with the row's newest version: an UPDATE takes its values from the
# first row joining it, a DELETE deletes it once.
#  (a) s1 re-keys the matched row onto another member row's key: the UPDATE adds that row's w
#  (b) s1 updates the matched row in place: the write runs over the newest version
#  (c) s1 deletes the matched row: no write
#  (d) s1 updates the member's row: the write reads the rows it saw and does not wait
#  (e) the DELETE over the re-keyed row, and the bare-member forms over it
#  (f) a large result relation probing the member's hash (the member a producer's Subquery Scan): the recheck rebuilds it
# The EXPLAIN steps pin the plans. Expected output is PostgreSQL 18.6's (the C oracle).
setup
{
 CREATE TABLE uo_t (id int PRIMARY KEY, v int NOT NULL, hits int NOT NULL DEFAULT 0);
 INSERT INTO uo_t SELECT i, i * 10, 0 FROM generate_series(1, 20) i;
 CREATE TABLE uo_s (k int, w int);
 INSERT INTO uo_s SELECT 100 + i, i FROM generate_series(1, 3000) i;
 INSERT INTO uo_s VALUES (5, 50), (25, 250);
 CREATE TABLE uo_b (id int PRIMARY KEY, v int NOT NULL, hits int NOT NULL DEFAULT 0);
 INSERT INTO uo_b SELECT i, i * 10, 0 FROM generate_series(1, 3000) i WHERE i <> 25;
 CREATE TABLE uo_m (k int, w int);
 INSERT INTO uo_m VALUES (5, 50), (25, 250), (9, 90), (9, 20);
 ANALYZE uo_t, uo_s, uo_b, uo_m;
}

teardown
{
 DROP TABLE uo_t, uo_s, uo_b, uo_m;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1key	{ UPDATE uo_t SET id = 25 WHERE id = 5; }
step s1v	{ UPDATE uo_t SET v = v + 1 WHERE id = 5; }
step s1del	{ DELETE FROM uo_t WHERE id = 5; }
step s1src	{ UPDATE uo_s SET w = 999 WHERE k = 5; }
step s1bkey	{ UPDATE uo_b SET id = 25 WHERE id = 5; }
step s1bv	{ UPDATE uo_b SET v = v + 1 WHERE id = 9; }
step s1c	{ COMMIT; }

session s2
setup		{ SET enable_nestloop = off; SET enable_mergejoin = off; }
step s2exp	{ EXPLAIN (COSTS OFF) UPDATE uo_t t SET hits = t.hits + s.w FROM (SELECT k, w FROM uo_s WHERE w > 0 UNION ALL SELECT 7, 1) s WHERE t.id = s.k AND s.w >= 50; }
step s2u	{ UPDATE uo_t t SET hits = t.hits + s.w FROM (SELECT k, w FROM uo_s WHERE w > 0 UNION ALL SELECT 7, 1) s WHERE t.id = s.k AND s.w >= 50; }
step s2d	{ DELETE FROM uo_t t USING (SELECT k, w FROM uo_s WHERE w > 0 UNION ALL SELECT 7, 1) s WHERE t.id = s.k AND s.w >= 50 RETURNING t.id, t.v; }
step s2bexp	{ EXPLAIN (COSTS OFF) UPDATE uo_t t SET hits = t.hits + s.w FROM (SELECT k, w FROM uo_s UNION ALL SELECT 7, 1) s WHERE t.id = s.k AND s.w >= 50; }
step s2ub	{ UPDATE uo_t t SET hits = t.hits + s.w FROM (SELECT k, w FROM uo_s UNION ALL SELECT 7, 1) s WHERE t.id = s.k AND s.w >= 50; }
step s2db	{ DELETE FROM uo_t t USING (SELECT k, w FROM uo_s UNION ALL SELECT 7, 1) s WHERE t.id = s.k AND s.w >= 50; }
step s2sel	{ SELECT id, v, hits FROM uo_t WHERE id IN (5, 7, 25) ORDER BY id; }
step s2pexp	{ EXPLAIN (COSTS OFF) UPDATE uo_b t SET hits = t.hits + s.w FROM (SELECT k, w FROM uo_m WHERE w > 0 UNION ALL SELECT 7, 1) s WHERE t.id = s.k AND s.w >= 50; }
step s2pu	{ UPDATE uo_b t SET hits = t.hits + s.w FROM (SELECT k, w FROM uo_m WHERE w > 0 UNION ALL SELECT 7, 1) s WHERE t.id = s.k AND s.w >= 50; }
step s2pd	{ DELETE FROM uo_b t USING (SELECT k, w FROM uo_m WHERE w > 0 UNION ALL SELECT 7, 1) s WHERE t.id = s.k AND s.w >= 50 RETURNING t.id, t.v; }
step s2psel	{ SELECT id, v, hits FROM uo_b WHERE id IN (5, 9, 25) ORDER BY id; }

permutation s2exp s1key s2u s1c s2sel
permutation s1v s2u s1c s2sel
permutation s1del s2u s1c s2sel
permutation s1src s2u s1c s2sel
permutation s1key s2d s1c s2sel
permutation s1v s2d s1c s2sel
permutation s2bexp s1key s2ub s1c s2sel
permutation s1v s2ub s1c s2sel
permutation s1key s2db s1c s2sel
permutation s2pexp s1bkey s2pu s1c s2psel
permutation s1bv s2pu s1c s2psel
permutation s1bkey s2pd s1c s2psel

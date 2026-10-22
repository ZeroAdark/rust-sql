# D3508: UPDATE .. FROM / DELETE .. USING, or MERGE with WHEN MATCHED actions only, over a flattened UNION ALL subquery
# (constant rows; a scanned member beside a constant arm) that PostgreSQL 18.6 plans as the outer of a nested loop whose
# inner is the target's index scan keyed by the union's column, while another session changes the matched row (READ
# COMMITTED). The subquery's COPY mark names a relation no scan carries, so EvalPlanQual runs the union's members again,
# whole, and rescans the target's index scan at each of their rows, handing it the row's newest version while that
# version passes the scan's index keys; the first key it fails clears it, so no later rescan returns it. The first joined
# row decides: an UPDATE takes its values from it, a MERGE calls the row MATCHED and acts with the ORIGINAL source row.
# The EXPLAIN steps pin the plans.
#  (a) s1 re-keys the matched row onto a later member row's key: the first rescan clears it, no write
#  (b) s1 updates the matched row in place: the write runs over the newest version
#  (c) s1 deletes the matched row: no write
#  (d) s1 re-keys it past every member row's key: no write
#  (e) the DELETE over the re-keyed row
#  (f) a scanned member beside a constant arm, the matched row updated in place
#  (g) two member rows on one key, a join qual the newest version fails for the first: the second rescan joins it
#  (h) MERGE over (a), (b) and (c), and a WHEN qual over the original source row
# Expected output is PostgreSQL 18.6's (the C oracle).
setup
{
 CREATE TABLE nr_t (id int PRIMARY KEY, v int NOT NULL, hits int NOT NULL DEFAULT 0);
 INSERT INTO nr_t SELECT i, i * 10, 0 FROM generate_series(1, 2000) i;
 CREATE TABLE nr_s (k int, w int);
 INSERT INTO nr_s SELECT 3000 + i, i FROM generate_series(1, 40) i;
 INSERT INTO nr_s VALUES (5, 50);
 ANALYZE nr_t, nr_s;
}

teardown
{
 DROP TABLE nr_t, nr_s;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1key	{ UPDATE nr_t SET id = 2500 WHERE id = 5; }
step s1far	{ UPDATE nr_t SET id = 2900 WHERE id = 5; }
step s1v	{ UPDATE nr_t SET v = v + 1 WHERE id = 5; }
step s1del	{ DELETE FROM nr_t WHERE id = 5; }
step s1c	{ COMMIT; }

session s2
step s2exp	{ EXPLAIN (COSTS OFF) UPDATE nr_t t SET hits = t.hits + s.w FROM (SELECT 5 AS k, 50 AS w UNION ALL SELECT 7, 1 UNION ALL SELECT 2500, 250) s WHERE t.id = s.k; }
step s2u	{ UPDATE nr_t t SET hits = t.hits + s.w FROM (SELECT 5 AS k, 50 AS w UNION ALL SELECT 7, 1 UNION ALL SELECT 2500, 250) s WHERE t.id = s.k; }
step s2d	{ DELETE FROM nr_t t USING (SELECT 5 AS k, 50 AS w UNION ALL SELECT 7, 1 UNION ALL SELECT 2500, 250) s WHERE t.id = s.k RETURNING t.id, t.v; }
step s2sexp	{ EXPLAIN (COSTS OFF) UPDATE nr_t t SET hits = t.hits + s.w FROM (SELECT k, w FROM nr_s WHERE w >= 50 UNION ALL SELECT 7, 1) s WHERE t.id = s.k; }
step s2us	{ UPDATE nr_t t SET hits = t.hits + s.w FROM (SELECT k, w FROM nr_s WHERE w >= 50 UNION ALL SELECT 7, 1) s WHERE t.id = s.k; }
step s2fexp	{ EXPLAIN (COSTS OFF) UPDATE nr_t t SET hits = t.hits + s.w FROM (SELECT 5 AS k, 49 AS w UNION ALL SELECT 5, 40 UNION ALL SELECT 7, 1) s WHERE t.id = s.k AND t.v + s.w < 100; }
step s2f	{ UPDATE nr_t t SET hits = t.hits + s.w FROM (SELECT 5 AS k, 49 AS w UNION ALL SELECT 5, 40 UNION ALL SELECT 7, 1) s WHERE t.id = s.k AND t.v + s.w < 100; }
step s2mexp	{ EXPLAIN (COSTS OFF) MERGE INTO nr_t t USING (SELECT 5 AS k, 50 AS w UNION ALL SELECT 7, 1 UNION ALL SELECT 2500, 250) s ON t.id = s.k WHEN MATCHED THEN UPDATE SET hits = t.hits + s.w; }
step s2m	{ MERGE INTO nr_t t USING (SELECT 5 AS k, 50 AS w UNION ALL SELECT 7, 1 UNION ALL SELECT 2500, 250) s ON t.id = s.k WHEN MATCHED THEN UPDATE SET hits = t.hits + s.w; }
step s2mq	{ MERGE INTO nr_t t USING (SELECT 5 AS k, 50 AS w UNION ALL SELECT 7, 1 UNION ALL SELECT 2500, 250) s ON t.id = s.k WHEN MATCHED AND s.w < 100 THEN DELETE WHEN MATCHED THEN UPDATE SET hits = 999; }
step s2sel	{ SELECT id, v, hits FROM nr_t WHERE id IN (5, 7, 2500, 2900) ORDER BY id; }

permutation s2exp s1key s2u s1c s2sel
permutation s1v s2u s1c s2sel
permutation s1del s2u s1c s2sel
permutation s1far s2u s1c s2sel
permutation s1key s2d s1c s2sel
permutation s1v s2d s1c s2sel
permutation s2sexp s1v s2us s1c s2sel
permutation s2fexp s1v s2f s1c s2sel
permutation s2mexp s1key s2m s1c s2sel
permutation s1v s2m s1c s2sel
permutation s1del s2m s1c s2sel
permutation s1v s2mq s1c s2sel

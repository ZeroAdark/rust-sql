# mergecmd D1685 (updatable_views#117, writes-0068, tw-core-148): MERGE ... WHEN NOT MATCHED BY SOURCE whose source is a
# VALUES list or a function (its row a ROW_MARK_COPY mark, carried whole), hashed on the LEFT join's nullable side, while
# another session changes a target row (READ COMMITTED). PostgreSQL 18.6's ExecMergeMatched rechecks the newest version
# under EvalPlanQual: a MATCHED row's source row is handed over whole, a NOT MATCHED BY SOURCE row's NULL wholerow gives
# the source no row at all (the source is never evaluated again), so the newest version can only be NOT MATCHED BY
# SOURCE. s2 turns nested loops and merge joins off; s2exp / s2expf pin the plans (Hash Left Join, the target scanned,
# the VALUES / Function Scan hashed).
#  (a) s1 updates a NOT MATCHED BY SOURCE row: its action runs on the newest version
#  (b) s1 moves that row key onto a key the VALUES list holds (2500): the recheck still has no source row, so the row
#      stays NOT MATCHED BY SOURCE and its qual (id <= 10) is now false: no action (a full rerun of the VALUES would
#      match 2500 and update it)
#  (c) s1 deletes that row: skipped
#  (d) s1 updates a MATCHED row: the MATCHED action runs on the newest version
#  (e) s1 makes the NOT MATCHED BY SOURCE qual false on the newest version (hits = 100): no action
#  (f) s1 aborts: the original version is updated (control)
#  (g) the function source with (a) and (b)
# Expected output is PostgreSQL 18.6's (the C oracle).
setup
{
 CREATE TABLE nc_t (id int PRIMARY KEY, v int NOT NULL, hits int NOT NULL DEFAULT 0);
 INSERT INTO nc_t SELECT i, i * 10, 0 FROM generate_series(1, 2000) i;
 ANALYZE nc_t;
}

teardown
{
 DROP TABLE nc_t;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1upd	{ UPDATE nc_t SET v = v + 1 WHERE id = 7; }
step s1key	{ UPDATE nc_t SET id = 2500 WHERE id = 7; }
step s1del	{ DELETE FROM nc_t WHERE id = 7; }
step s1mat	{ UPDATE nc_t SET v = v + 1 WHERE id = 5; }
step s1qual	{ UPDATE nc_t SET hits = 100 WHERE id = 7; }
step s1c	{ COMMIT; }
step s1a	{ ROLLBACK; }

session s2
setup		{ SET enable_nestloop = off; SET enable_mergejoin = off; }
step s2exp	{ EXPLAIN (COSTS OFF) MERGE INTO nc_t t USING (VALUES (3, 30), (5, 50), (2500, 1)) AS s(k, w) ON t.id = s.k WHEN MATCHED THEN UPDATE SET hits = t.hits + s.w WHEN NOT MATCHED BY SOURCE AND t.id <= 10 AND t.hits < 50 THEN UPDATE SET hits = t.hits + 1000, v = t.v + 1; }
step s2mrg	{ MERGE INTO nc_t t USING (VALUES (3, 30), (5, 50), (2500, 1)) AS s(k, w) ON t.id = s.k WHEN MATCHED THEN UPDATE SET hits = t.hits + s.w WHEN NOT MATCHED BY SOURCE AND t.id <= 10 AND t.hits < 50 THEN UPDATE SET hits = t.hits + 1000, v = t.v + 1; }
step s2expf	{ EXPLAIN (COSTS OFF) MERGE INTO nc_t t USING (SELECT g * 1250 AS k FROM generate_series(2, 3) g) s ON t.id = s.k WHEN MATCHED THEN UPDATE SET hits = t.hits + 7 WHEN NOT MATCHED BY SOURCE AND t.id <= 10 THEN DELETE; }
step s2mrgf	{ MERGE INTO nc_t t USING (SELECT g * 1250 AS k FROM generate_series(2, 3) g) s ON t.id = s.k WHEN MATCHED THEN UPDATE SET hits = t.hits + 7 WHEN NOT MATCHED BY SOURCE AND t.id <= 10 THEN DELETE; }
step s2sel	{ SELECT id, v, hits FROM nc_t WHERE id <= 10 OR id >= 2000 ORDER BY id; }

permutation s2exp s1upd s2mrg s1c s2sel
permutation s1key s2mrg s1c s2sel
permutation s1del s2mrg s1c s2sel
permutation s1mat s2mrg s1c s2sel
permutation s1qual s2mrg s1c s2sel
permutation s1upd s2mrg s1a s2sel
permutation s2expf s1upd s2mrgf s1c s2sel
permutation s1key s2mrgf s1c s2sel

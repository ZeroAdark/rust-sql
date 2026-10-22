# D1685 (orm-0357's shape): MERGE ... WHEN NOT MATCHED BY SOURCE whose source is a flattened UNION ALL of constant rows
# (each arm a Result node; the union's row a ROW_MARK_COPY mark that no scan takes), hashed (the target probes), while
# another session changes a target row (READ COMMITTED). PostgreSQL 18.6's ExecMergeMatched rechecks a MATCHED row under
# EvalPlanQual, whose first row is the target's (it probes), and then decides by the join condition
# (ri_MergeJoinCondition) over the newest version and the ORIGINAL source row, which its actions read too: the newest
# version never matches another arm. A NOT MATCHED BY SOURCE row is never rechecked against the source, so it stays NOT
# MATCHED BY SOURCE. s2 turns nested loops and merge joins off; the EXPLAIN steps pin the plans (the target scanned, the
# Append of Results hashed).
#  (a) s1 updates a NOT MATCHED BY SOURCE row: its action runs on the newest version
#  (b) s1 moves that row's key onto a key the union holds (2500): it stays NOT MATCHED BY SOURCE and its qual
#      (id <= 10) is now false: no action
#  (c) s1 deletes that row: skipped
#  (d) s1 updates a MATCHED row: the MATCHED action runs on the newest version
#  (e) s1 moves a MATCHED row's key onto another arm's key (2500): the original source row (5) no longer matches and
#      the NOT MATCHED BY SOURCE qual is false on the newest version: no action
#  (f) s1 makes the NOT MATCHED BY SOURCE qual false on the newest version (hits = 100): no action
#  (g) s1 aborts: the original version is updated (control)
#  (h) the full join form (MATCHED, NOT MATCHED and NOT MATCHED BY SOURCE actions) with (a), (d) and a MATCHED row
#      moved off every key (2600): the original source row no longer matches, so the NOT MATCHED action inserts it (5)
#      and the moved row's NOT MATCHED BY SOURCE qual is false
# Expected output is PostgreSQL 18.6's (the C oracle).
setup
{
 CREATE TABLE cu_t (id int PRIMARY KEY, v int NOT NULL, hits int NOT NULL DEFAULT 0);
 INSERT INTO cu_t SELECT i, i * 10, 0 FROM generate_series(1, 2000) i;
 ANALYZE cu_t;
}

teardown
{
 DROP TABLE cu_t;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1upd	{ UPDATE cu_t SET v = v + 1 WHERE id = 7; }
step s1key	{ UPDATE cu_t SET id = 2500 WHERE id = 7; }
step s1del	{ DELETE FROM cu_t WHERE id = 7; }
step s1mat	{ UPDATE cu_t SET v = v + 1 WHERE id = 5; }
step s1matkey	{ UPDATE cu_t SET id = 2500 WHERE id = 5; }
step s1matoff	{ UPDATE cu_t SET id = 2600 WHERE id = 5; }
step s1qual	{ UPDATE cu_t SET hits = 100 WHERE id = 7; }
step s1c	{ COMMIT; }
step s1a	{ ROLLBACK; }

session s2
setup		{ SET enable_nestloop = off; SET enable_mergejoin = off; }
step s2exp	{ EXPLAIN (COSTS OFF) MERGE INTO cu_t t USING (SELECT 3 AS k, 30 AS w UNION ALL SELECT 5, 50 UNION ALL SELECT 2500, 1) s ON t.id = s.k WHEN MATCHED THEN UPDATE SET hits = t.hits + s.w WHEN NOT MATCHED BY SOURCE AND t.id <= 10 AND t.hits < 50 THEN UPDATE SET hits = t.hits + 1000, v = t.v + 1; }
step s2mrg	{ MERGE INTO cu_t t USING (SELECT 3 AS k, 30 AS w UNION ALL SELECT 5, 50 UNION ALL SELECT 2500, 1) s ON t.id = s.k WHEN MATCHED THEN UPDATE SET hits = t.hits + s.w WHEN NOT MATCHED BY SOURCE AND t.id <= 10 AND t.hits < 50 THEN UPDATE SET hits = t.hits + 1000, v = t.v + 1; }
step s2expo	{ EXPLAIN (COSTS OFF) MERGE INTO cu_t t USING (SELECT 3 AS k, 30 AS w UNION ALL SELECT 5, 50 UNION ALL SELECT 4000, 1) s ON t.id = s.k WHEN MATCHED THEN UPDATE SET hits = t.hits + s.w WHEN NOT MATCHED THEN INSERT (id, v, hits) VALUES (s.k, s.w, -1) WHEN NOT MATCHED BY SOURCE AND t.id <= 10 THEN UPDATE SET hits = t.hits + 1000; }
step s2mrgo	{ MERGE INTO cu_t t USING (SELECT 3 AS k, 30 AS w UNION ALL SELECT 5, 50 UNION ALL SELECT 4000, 1) s ON t.id = s.k WHEN MATCHED THEN UPDATE SET hits = t.hits + s.w WHEN NOT MATCHED THEN INSERT (id, v, hits) VALUES (s.k, s.w, -1) WHEN NOT MATCHED BY SOURCE AND t.id <= 10 THEN UPDATE SET hits = t.hits + 1000; }
step s2sel	{ SELECT id, v, hits FROM cu_t WHERE id <= 10 OR id >= 2000 ORDER BY id; }

permutation s2exp s1upd s2mrg s1c s2sel
permutation s1key s2mrg s1c s2sel
permutation s1del s2mrg s1c s2sel
permutation s1mat s2mrg s1c s2sel
permutation s1matkey s2mrg s1c s2sel
permutation s1qual s2mrg s1c s2sel
permutation s1upd s2mrg s1a s2sel
permutation s2expo s1upd s2mrgo s1c s2sel
permutation s1mat s2mrgo s1c s2sel
permutation s1matoff s2mrgo s1c s2sel

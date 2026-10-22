# D3466 (D1685 forms): MERGE ... WHEN NOT MATCHED BY SOURCE whose source is a flattened UNION ALL hashed under the join (the
# target probes) with a member that is not a constant row (a scanned relation under a qual, an arm gated off by a
# constant qual), while another session changes a target row (READ COMMITTED). The union's row is one ROW_MARK_COPY
# mark on the union itself (its members carry no mark). PostgreSQL 18.6's ExecMergeMatched rechecks a MATCHED row
# under EvalPlanQual and then decides by the join condition (ri_MergeJoinCondition) over the newest version and the
# ORIGINAL source row, which its actions read too, whatever member produced it. A NOT MATCHED BY SOURCE row is never
# rechecked against the source.
# s2 turns nested loops and merge joins off; the EXPLAIN step pins the first form's plan (the target scanned, the
# Append hashed).
#  (a) s1 updates a NOT MATCHED BY SOURCE row: its action runs on the newest version
#  (b) s1 updates a row the scanned member matched: the MATCHED action runs on the newest version with that member's row
#  (c) s1 moves that row's key onto the constant arm's key (2500): the original source row (5) no longer matches and the
#      NOT MATCHED BY SOURCE qual is false on the newest version: no action
#  (d) s1 moves the scanned member's row away (uf_src, not yet committed): the MERGE reads the old row and does not wait
#  (e) the gated form (a constant arm under WHERE false beside a scanned member) with (b) and (c)
# Expected output is PostgreSQL 18.6's (the C oracle).
setup
{
 CREATE TABLE uf_t (id int PRIMARY KEY, v int NOT NULL, hits int NOT NULL DEFAULT 0);
 INSERT INTO uf_t SELECT i, i * 10, 0 FROM generate_series(1, 2000) i;
 CREATE TABLE uf_src (k int, w int);
 INSERT INTO uf_src VALUES (3, 30), (5, 50);
 ANALYZE uf_t, uf_src;
}

teardown
{
 DROP TABLE uf_t, uf_src;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1upd	{ UPDATE uf_t SET v = v + 1 WHERE id = 7; }
step s1mat	{ UPDATE uf_t SET v = v + 1 WHERE id = 5; }
step s1matkey	{ UPDATE uf_t SET id = 2500 WHERE id = 5; }
step s1src	{ UPDATE uf_src SET k = 9 WHERE k = 5; }
step s1c	{ COMMIT; }

session s2
setup		{ SET enable_nestloop = off; SET enable_mergejoin = off; }
step s2exp	{ EXPLAIN (COSTS OFF) MERGE INTO uf_t t USING (SELECT k, w FROM uf_src WHERE w > 0 UNION ALL SELECT 2500, 1) s ON t.id = s.k WHEN MATCHED THEN UPDATE SET hits = t.hits + s.w WHEN NOT MATCHED BY SOURCE AND t.id <= 10 AND t.hits < 50 THEN UPDATE SET hits = t.hits + 1000, v = t.v + 1; }
step s2mrg	{ MERGE INTO uf_t t USING (SELECT k, w FROM uf_src WHERE w > 0 UNION ALL SELECT 2500, 1) s ON t.id = s.k WHEN MATCHED THEN UPDATE SET hits = t.hits + s.w WHEN NOT MATCHED BY SOURCE AND t.id <= 10 AND t.hits < 50 THEN UPDATE SET hits = t.hits + 1000, v = t.v + 1; }
step s2mrgg	{ MERGE INTO uf_t t USING (SELECT k, w FROM uf_src WHERE w > 0 UNION ALL SELECT 2500, 1 WHERE false UNION ALL SELECT 2600, 2) s ON t.id = s.k WHEN MATCHED THEN UPDATE SET hits = t.hits + s.w WHEN NOT MATCHED BY SOURCE AND t.id <= 10 THEN UPDATE SET hits = t.hits + 1000; }
step s2sel	{ SELECT id, v, hits FROM uf_t WHERE id <= 10 OR id >= 2000 ORDER BY id; }

permutation s2exp s1upd s2mrg s1c s2sel
permutation s1mat s2mrg s1c s2sel
permutation s1matkey s2mrg s1c s2sel
permutation s1src s2mrg s1c s2sel
permutation s1mat s2mrgg s1c s2sel
permutation s1matkey s2mrgg s1c s2sel

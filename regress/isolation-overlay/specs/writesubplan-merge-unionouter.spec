# writesubplan D2015 UNION-OUTER (held-out parts-0083): a MERGE through a partitioned root over a Nested Loop Left Join
# whose outer is a flattened UNION ALL of constant rows (an Append of Results) and whose inner is the Append of the
# leaves' index scans (every leaf a result relation, one with another column order and a dropped column), a MATCHED
# UPDATE beside the NOT MATCHED INSERT routed through the root, while another session changes a matched row (READ
# COMMITTED). PostgreSQL 18.6's EvalPlanQual hands the union's COPY mark to no scan: it evaluates the union's arms again
# from the first and takes the first joined row, the first arm's row joined to the newest version through the row's own
# leaf (the other leaves blocked). ExecMergeMatched decides MATCHED by that row's ctid, then runs the actions with the
# stopped row's source columns (planSlot): the UPDATE's region and the INSERT read v.id of the row s2 stopped at.
#  (a) s1 updates the second arm's row: the first arm's row does not join its newest version, so NOT MATCHED: the
#      stopped row's INSERT (a second (1, 2022-12-08) row beside the updated one)
#  (b) s1 updates the first arm's row (in the leaf with the dropped column): MATCHED, the UPDATE over the newest version
#  (c) s1 deletes the second arm's row: NOT MATCHED, the stopped row's INSERT
#  (d) s1 re-keys the second arm's row to the first arm's key within its leaf: the first arm's row joins it, so MATCHED,
#      the UPDATE over the newest version with the stopped row's v.id in region
#  (e) s1 moves the second arm's row to the other leaf: 40001
#  (f) s1 aborts: the original version (control)
#  (g) (a) and (d) with RETURNING merge_action(), v.id and the row
# s2 plans with nested loops alone; each permutation ends its transaction. Expected output is PostgreSQL 18.6's (the C oracle).
setup
{
 CREATE TABLE uo_t (id int NOT NULL, d date NOT NULL, qty int, region text) PARTITION BY RANGE (d);
 CREATE TABLE uo_t1 PARTITION OF uo_t FOR VALUES FROM ('2022-01-01') TO ('2023-01-01');
 CREATE TABLE uo_t2 (region text, x int, qty int, d date NOT NULL, id int NOT NULL);
 ALTER TABLE uo_t2 DROP x;
 ALTER TABLE uo_t ATTACH PARTITION uo_t2 FOR VALUES FROM ('2023-01-01') TO ('2024-01-01');
 CREATE INDEX ON uo_t (id, d);
 INSERT INTO uo_t SELECT g, date '2022-01-01' + (g % 700), 1, 'x' FROM generate_series(2, 3000) g;
 INSERT INTO uo_t VALUES (1, '2022-12-08', 5, 'r1'), (900010, '2023-03-03', 7, 'r9');
 ANALYZE uo_t;
}

teardown
{
 DROP TABLE uo_t;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1u1	{ UPDATE uo_t SET region = 'upd' WHERE id = 1; }
step s1u9	{ UPDATE uo_t SET region = 'upd' WHERE id = 900010; }
step s1d1	{ DELETE FROM uo_t WHERE id = 1; }
step s1k1	{ UPDATE uo_t SET id = 900011, d = '2022-12-09' WHERE id = 1; }
step s1mv	{ UPDATE uo_t SET d = '2023-05-05' WHERE id = 1; }
step s1c	{ COMMIT; }
step s1a	{ ROLLBACK; }

session s2
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; SET LOCAL enable_hashjoin = off; SET LOCAL enable_mergejoin = off; }
step s2e	{ EXPLAIN (COSTS OFF) MERGE INTO uo_t s USING (SELECT 900010 AS id, date '2023-03-03' AS d UNION ALL SELECT 1, date '2022-12-08') v ON s.id = v.id AND s.d = v.d WHEN MATCHED THEN UPDATE SET qty = qty + 100, region = s.region || ' m' || v.id WHEN NOT MATCHED THEN INSERT (id, d, region) VALUES (v.id, v.d, 'ins' || v.id); }
step s2m	{ MERGE INTO uo_t s USING (SELECT 900010 AS id, date '2023-03-03' AS d UNION ALL SELECT 1, date '2022-12-08') v ON s.id = v.id AND s.d = v.d WHEN MATCHED THEN UPDATE SET qty = qty + 100, region = s.region || ' m' || v.id WHEN NOT MATCHED THEN INSERT (id, d, region) VALUES (v.id, v.d, 'ins' || v.id); }
step s2k	{ MERGE INTO uo_t s USING (SELECT 900011 AS id, date '2022-12-09' AS d UNION ALL SELECT 1, date '2022-12-08') v ON s.id = v.id AND s.d = v.d WHEN MATCHED THEN UPDATE SET qty = qty + 100, region = s.region || ' m' || v.id WHEN NOT MATCHED THEN INSERT (id, d, region) VALUES (v.id, v.d, 'ins' || v.id); }
step s2r	{ MERGE INTO uo_t s USING (SELECT 900010 AS id, date '2023-03-03' AS d UNION ALL SELECT 1, date '2022-12-08') v ON s.id = v.id AND s.d = v.d WHEN MATCHED THEN UPDATE SET qty = qty + 100, region = s.region || ' m' || v.id WHEN NOT MATCHED THEN INSERT (id, d, region) VALUES (v.id, v.d, 'ins' || v.id) RETURNING merge_action(), v.id, s.*; }
step s2kr	{ MERGE INTO uo_t s USING (SELECT 900011 AS id, date '2022-12-09' AS d UNION ALL SELECT 1, date '2022-12-08') v ON s.id = v.id AND s.d = v.d WHEN MATCHED THEN UPDATE SET qty = qty + 100, region = s.region || ' m' || v.id WHEN NOT MATCHED THEN INSERT (id, d, region) VALUES (v.id, v.d, 'ins' || v.id) RETURNING merge_action(), v.id, s.*; }
step s2sel	{ SELECT tableoid::regclass, id, d, qty, region FROM uo_t WHERE id IN (1, 900010, 900011) ORDER BY id, d, qty NULLS FIRST, region; }
step s2c	{ COMMIT; }

permutation s2e s2c s1c
permutation s1u1 s2m s1c s2sel s2c
permutation s1u9 s2m s1c s2sel s2c
permutation s1d1 s2m s1c s2sel s2c
permutation s1k1 s2k s1c s2sel s2c
permutation s1mv s2m s1c s2sel s2c
permutation s1u1 s2m s1a s2sel s2c
permutation s1u1 s2r s1c s2sel s2c
permutation s1k1 s2kr s1c s2sel s2c

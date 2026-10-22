# writesubplan D2015 NEST-ARMS (census e2e-ald-write_route-195, held-out parts-0083): a MERGE through a partitioned root
# over a Nested Loop Left Join whose outer is a VALUES source and whose inner is the Append of the leaves' index scans
# (every leaf a result relation, one with another column order and a dropped column), a MATCHED UPDATE and DELETE
# beside the NOT MATCHED INSERT routed through the root, while another session changes a matched row (READ COMMITTED).
# PostgreSQL 18.6's EvalPlanQual substitutes the row's own leaf scan with the newest version and blocks the other
# leaves' scans (relsubs_blocked), hands the VALUES scan its stored row and reruns the join and the action list:
#  (a) s1 updates the row's val: the UPDATE runs over the newest version
#  (b) s1 makes the first action's WHEN false on the newest version (n < 0): the second MATCHED action (DELETE) runs
#  (c) s1 deletes the row: NOT MATCHED, the source row's INSERT routed through the root
#  (d) s1 changes the row's key within its leaf (k 10 -> 12): the newest version fails the arm's index key, NOT MATCHED
#  (e) s1 moves the row to the other leaf: 40001
#  (f) s1 aborts: the original version (control)
#  (g) (a) and (c) with RETURNING merge_action(), tableoid and the row (write_route-195's form)
#  (h) source rows matching a row in each leaf, s1 updating both: each rechecked in its own leaf
#  (i) the same form over a small tree whose arms are sequential scans filtering the key (nh_t), s3 with s1's (a), (c)
#      and (d)
# s2 and s3 plan with nested loops alone (s3 in autocommit; each permutation ends s2's transaction). Expected output is PostgreSQL 18.6's (the C oracle).
setup
{
 CREATE TABLE na_t (k int PRIMARY KEY, val text, n int) PARTITION BY RANGE (k);
 CREATE TABLE na_t1 PARTITION OF na_t FOR VALUES FROM (1) TO (10000);
 CREATE TABLE na_t2 (val text, x int, n int, k int NOT NULL);
 ALTER TABLE na_t2 DROP x;
 ALTER TABLE na_t ATTACH PARTITION na_t2 FOR VALUES FROM (10000) TO (20000);
 INSERT INTO na_t SELECT g, 'v' || g, g FROM generate_series(1, 19999) g WHERE g % 3 <> 0;
 CREATE TABLE nh_t (k int PRIMARY KEY, val text, n int) PARTITION BY RANGE (k);
 CREATE TABLE nh_t1 PARTITION OF nh_t FOR VALUES FROM (1) TO (100);
 CREATE TABLE nh_t2 PARTITION OF nh_t FOR VALUES FROM (100) TO (200);
 INSERT INTO nh_t SELECT g, 'v' || g, g FROM generate_series(1, 199) g WHERE g % 3 <> 0;
 ANALYZE na_t, nh_t;
}

teardown
{
 DROP TABLE na_t, nh_t;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1upd	{ UPDATE na_t SET val = 'a1' WHERE k = 10; }
step s1neg	{ UPDATE na_t SET n = -1 WHERE k = 10; }
step s1del	{ DELETE FROM na_t WHERE k = 10; }
step s1key	{ UPDATE na_t SET k = 12 WHERE k = 10; }
step s1mv	{ UPDATE na_t SET k = 10149 WHERE k = 10; }
step s1both	{ UPDATE na_t SET val = val || '!' WHERE k IN (10, 10110); }
step s1hu	{ UPDATE nh_t SET val = 'a1' WHERE k = 10; }
step s1hd	{ DELETE FROM nh_t WHERE k = 10; }
step s1hk	{ UPDATE nh_t SET k = 12 WHERE k = 10; }
step s1c	{ COMMIT; }
step s1a	{ ROLLBACK; }

session s2
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; SET LOCAL enable_hashjoin = off; SET LOCAL enable_mergejoin = off; }
step s2e	{ EXPLAIN (COSTS OFF) MERGE INTO na_t t USING (VALUES (10, 1), (51, 2)) s(a, b) ON t.k = s.a WHEN MATCHED AND t.n >= 0 THEN UPDATE SET val = t.val || ' m', n = t.n + s.b WHEN MATCHED THEN DELETE WHEN NOT MATCHED THEN INSERT VALUES (s.a, 'ins', s.b); }
step s2m	{ MERGE INTO na_t t USING (VALUES (10, 1), (51, 2)) s(a, b) ON t.k = s.a WHEN MATCHED AND t.n >= 0 THEN UPDATE SET val = t.val || ' m', n = t.n + s.b WHEN MATCHED THEN DELETE WHEN NOT MATCHED THEN INSERT VALUES (s.a, 'ins', s.b); }
step s2r	{ MERGE INTO na_t t USING (VALUES (10, 1), (51, 2)) s(a, b) ON t.k = s.a WHEN MATCHED AND t.n >= 0 THEN UPDATE SET val = t.val || ' m', n = t.n + s.b WHEN MATCHED THEN DELETE WHEN NOT MATCHED THEN INSERT VALUES (s.a, 'ins', s.b) RETURNING merge_action(), tableoid::regclass, t.*; }
step s2two	{ MERGE INTO na_t t USING (VALUES (10, 1), (10110, 2)) s(a, b) ON t.k = s.a WHEN MATCHED THEN UPDATE SET val = t.val || ' m' WHEN NOT MATCHED THEN INSERT VALUES (s.a, 'ins', s.b); }
step s2sel	{ SELECT tableoid::regclass, k, val, n FROM na_t WHERE k IN (10, 12, 51, 10110, 10149) ORDER BY k; }
step s2c	{ COMMIT; }

session s3
setup		{ SET enable_hashjoin = off; SET enable_mergejoin = off; }
step s3e	{ EXPLAIN (COSTS OFF) MERGE INTO nh_t t USING (VALUES (10, 1), (51, 2)) s(a, b) ON t.k = s.a WHEN MATCHED THEN UPDATE SET val = t.val || ' m', n = t.n + s.b WHEN NOT MATCHED THEN INSERT VALUES (s.a, 'ins', s.b); }
step s3m	{ MERGE INTO nh_t t USING (VALUES (10, 1), (51, 2)) s(a, b) ON t.k = s.a WHEN MATCHED THEN UPDATE SET val = t.val || ' m', n = t.n + s.b WHEN NOT MATCHED THEN INSERT VALUES (s.a, 'ins', s.b); }
step s3sel	{ SELECT tableoid::regclass, k, val, n FROM nh_t WHERE k IN (10, 12, 51) ORDER BY k, val; }

permutation s2e s1upd s2m s1c s2sel s2c
permutation s1neg s2m s1c s2sel s2c
permutation s1del s2m s1c s2sel s2c
permutation s1key s2m s1c s2sel s2c
permutation s1mv s2m s1c s2sel s2c
permutation s1upd s2m s1a s2sel s2c
permutation s1upd s2r s1c s2sel s2c
permutation s1del s2r s1c s2sel s2c
permutation s1both s2two s1c s2sel s2c
permutation s3e s1hu s3m s1c s3sel s2c
permutation s1hd s3m s1c s3sel s2c
permutation s1hk s3m s1c s3sel s2c

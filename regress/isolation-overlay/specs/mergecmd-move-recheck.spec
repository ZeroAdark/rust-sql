# mergecmd MOVE (merge#377 / #425, triggers#530): a MERGE through a partitioned root whose UPDATE action moves the
# matched row to another leaf, while another session changes that row (READ COMMITTED). PostgreSQL 18.6's
# ExecCrossPartitionUpdate deletes the row from its leaf first; a delete that meets a newer version leaves the retry to
# ExecMergeMatched, which locks the newest version and reruns the join qual and the whole action list over it.
#  (a) s1 updates the row's val: the move runs over the newest version (its val)
#  (b) s1 makes the first action's WHEN false on the newest version: the second action (DELETE) runs instead
#  (c) s1 deletes the row: NOT MATCHED, nothing done (no NOT MATCHED action)
#  (d) s1 moves the row to another leaf itself: the lock finds it moved (40001)
#  (e) s1 aborts: the original version moves (control)
#  (f) a source matching rows of two leaves with (a)
#  (g) mergecmd MOVE (c): one result relation, the pruned leaf the outer of a nested loop (a key pinned by a constant:
#      merge#377's form; s3 plans with nested loops alone, each statement its own READ COMMITTED transaction) with (a),
#      (b), (c) and (d), and s1 changing the source row beside (a): EvalPlanQual refetches the version the join read
#      (EvalPlanQualFetchRowMark), which still joins. Each ends with s2c, closing the transaction s2's setup opened
#  (h) mergecmd MOVE (b): merge#377 itself over tables never analyzed (mu_t, mu_s): the source's scan the outer, the
#      pruned leaf under a Materialize inner; s4 moves the row with s1 updating it (a), changing its WHEN (b), deleting it
#      (c), moving it (d)
#  (i) mergecmd TABLEOID-HASH: merge#425's form over the same tables: two result relations (mu_t1, mu_t2) under the Hash
#      the source's scan probes; s5 moves one row and deletes another with s1's (a), (b), (c) and (d) on the moving row
#  (j) mergecmd SORT-UNION (merge#364 group): a NOT MATCHED INSERT beside the UPDATE / DELETE over the same tables (a
#      Merge Left Join, the Sort over the result relations' Append the inner); s6 updates or deletes the matched rows and
#      inserts a source row, with s1's (a), (b), (c: the concurrently deleted row falls through to the NOT MATCHED
#      INSERT, routed through the root) and (d) on tid 1
#  (k) mergecmd SORT-UNION part 2: NOT MATCHED BY SOURCE beside them (a Merge Full Join); s7 with s1 updating a matched
#      row (a) or deleting it (c), updating or deleting the BY SOURCE row (tid 15: EvalPlanQual's join over the newest
#      version and no source row gives that row again, its BY SOURCE action runs over it; deleted, the NOT MATCHED
#      actions see a NULL source), and moving a matched row's key within its leaf (tid 1 -> 4: the recheck's first joined
#      row is the source row alone, NOT MATCHED)
#  (l) held-out tw-part-022's form: a one-row VALUES source pins the leaf mr_t1 (one result relation, the leaf's scan the
#      inner of a Nested Loop Left Join, a Result the outer), MATCHED UPDATE beside the NOT MATCHED INSERT routed through
#      the root (s3vm): s1 updates the row (a: the UPDATE reads the newest version), deletes it (c: NOT MATCHED, the source
#      row's INSERT routed to mr_t2), moves it to another leaf (d: 40001), or aborts (control)
# The root holds 2000 more rows in its DEFAULT leaf and s2 plans with hash joins alone, so PostgreSQL hashes the source and
# probes the leaves' Append (the move served over several result relations).
# Expected output is PostgreSQL 18.6's (the C oracle).
setup
{
 CREATE TABLE mr_t (k int, val text, n int) PARTITION BY LIST (k);
 CREATE TABLE mr_t1 PARTITION OF mr_t FOR VALUES IN (1, 3);
 CREATE TABLE mr_t2 (n int, x int, val text, k int);
 ALTER TABLE mr_t2 DROP COLUMN x;
 ALTER TABLE mr_t ATTACH PARTITION mr_t2 FOR VALUES IN (2, 4);
 CREATE TABLE mr_td PARTITION OF mr_t DEFAULT;
 INSERT INTO mr_t VALUES (1, 'a', 10), (2, 'b', 20), (3, 'c', 30);
 INSERT INTO mr_t SELECT g, 'z', g FROM generate_series(100, 2099) g;
 CREATE TABLE mr_s (k int);
 INSERT INTO mr_s VALUES (1);
 CREATE TABLE mr_s2 (k int);
 INSERT INTO mr_s2 VALUES (1), (2);
 ANALYZE mr_t, mr_s, mr_s2;
 CREATE TABLE mu_t (tid integer, balance float, val text) PARTITION BY LIST (tid);
 CREATE TABLE mu_t1 PARTITION OF mu_t FOR VALUES IN (1, 4) WITH (autovacuum_enabled = off);
 CREATE TABLE mu_t2 PARTITION OF mu_t FOR VALUES IN (2, 5, 6) WITH (autovacuum_enabled = off);
 CREATE TABLE mu_t3 PARTITION OF mu_t DEFAULT WITH (autovacuum_enabled = off);
 CREATE TABLE mu_s (sid integer, delta float);
 INSERT INTO mu_s SELECT id, id * 10 FROM generate_series(1, 14) AS id;
 INSERT INTO mu_t SELECT id, id * 100, 'initial' FROM generate_series(1, 15, 2) AS id;
}

teardown
{
 DROP TABLE mr_t, mr_s, mr_s2, mu_t, mu_s;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1upd	{ UPDATE mr_t SET val = 'a1' WHERE k = 1; }
step s1n	{ UPDATE mr_t SET n = 99 WHERE k = 1; }
step s1del	{ DELETE FROM mr_t WHERE k = 1; }
step s1mv	{ UPDATE mr_t SET k = 2 WHERE k = 1; }
step s1upd2	{ UPDATE mr_t SET val = val || '!' WHERE k IN (1, 2); }
step s1both	{ UPDATE mr_t SET val = 'a1' WHERE k = 1; UPDATE mr_s SET k = 3 WHERE k = 1; }
step s1uu	{ UPDATE mu_t SET val = 'changed' WHERE tid = 1; }
step s1ub	{ UPDATE mu_t SET balance = 999 WHERE tid = 1; }
step s1ud	{ DELETE FROM mu_t WHERE tid = 1; }
step s1um	{ UPDATE mu_t SET tid = 2 WHERE tid = 1; }
step s1u15	{ UPDATE mu_t SET val = 'changed' WHERE tid = 15; }
step s1d15	{ DELETE FROM mu_t WHERE tid = 15; }
step s1k	{ UPDATE mu_t SET tid = 4 WHERE tid = 1; }
step s1c	{ COMMIT; }
step s1a	{ ROLLBACK; }

session s2
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; SET LOCAL enable_nestloop = off; SET LOCAL enable_mergejoin = off; }
step s2m	{ MERGE INTO mr_t t USING mr_s s ON t.k = s.k WHEN MATCHED AND t.n < 50 THEN UPDATE SET k = 4, val = t.val || ' moved' WHEN MATCHED THEN DELETE; }
step s2m2	{ MERGE INTO mr_t t USING mr_s2 s ON t.k = s.k WHEN MATCHED THEN UPDATE SET k = t.k + 4, val = t.val || ' moved'; }
step s2sel	{ SELECT tableoid::regclass, k, val, n FROM mr_t WHERE k < 100 ORDER BY k, val; }
step s2c	{ COMMIT; }

session s3
setup		{ SET enable_hashjoin = off; SET enable_mergejoin = off; }
step s3p	{ MERGE INTO mr_t t USING mr_s s ON t.k = s.k AND t.k = 1 WHEN MATCHED AND t.n < 50 THEN UPDATE SET k = 4, val = t.val || ' moved' WHEN MATCHED THEN DELETE; }
step s3pv	{ MERGE INTO mr_t t USING mr_s s ON t.k = s.k AND t.k = 1 WHEN MATCHED THEN UPDATE SET val = t.val || ' kept'; }
step s3sel	{ SELECT tableoid::regclass, k, val, n FROM mr_t WHERE k < 100 ORDER BY k, val; }
step s3ve	{ EXPLAIN (COSTS OFF) MERGE INTO mr_t t USING (VALUES (1, 'v')) s(k, val) ON t.k = s.k WHEN MATCHED THEN UPDATE SET val = t.val || ' ' || s.val WHEN NOT MATCHED THEN INSERT VALUES (s.k + 1, 'ins', 0); }
step s3vm	{ MERGE INTO mr_t t USING (VALUES (1, 'v')) s(k, val) ON t.k = s.k WHEN MATCHED THEN UPDATE SET val = t.val || ' ' || s.val WHEN NOT MATCHED THEN INSERT VALUES (s.k + 1, 'ins', 0); }

session s4
step s4m	{ MERGE INTO mu_t t USING mu_s s ON t.tid = s.sid AND t.tid = 1 WHEN MATCHED AND t.balance < 500 THEN UPDATE SET tid = tid + 1, balance = balance + delta, val = val || ' updated by merge' WHEN MATCHED THEN DELETE; }
step s4sel	{ SELECT tableoid::regclass, * FROM mu_t ORDER BY tid, val; }

session s5
step s5h	{ MERGE INTO mu_t t USING mu_s s ON t.tid = s.sid AND t.tid IN (1, 2, 4, 5) WHEN MATCHED AND t.balance < 500 THEN UPDATE SET tid = tid + 1, val = val || ' moved' WHEN MATCHED THEN DELETE; }

session s7
step s7f	{ MERGE INTO mu_t t USING mu_s s ON t.tid = s.sid WHEN MATCHED THEN UPDATE SET val = val || ' updated' WHEN NOT MATCHED AND s.sid < 4 THEN INSERT VALUES (sid, delta, 'inserted') WHEN NOT MATCHED BY SOURCE THEN UPDATE SET val = val || ' by source'; }

session s6
step s6u	{ MERGE INTO mu_t t USING mu_s s ON t.tid = s.sid WHEN MATCHED AND t.balance < 500 THEN UPDATE SET val = val || ' updated' WHEN MATCHED THEN DELETE WHEN NOT MATCHED AND s.sid < 4 THEN INSERT VALUES (sid, delta, 'inserted'); }

permutation s1upd s2m s1c s2sel s2c
permutation s1n s2m s1c s2sel s2c
permutation s1del s2m s1c s2sel s2c
permutation s1mv s2m s1c s2sel s2c
permutation s1upd s2m s1a s2sel s2c
permutation s1upd2 s2m2 s1c s2sel s2c
permutation s1upd s3p s1c s3sel s2c
permutation s1n s3p s1c s3sel s2c
permutation s1del s3p s1c s3sel s2c
permutation s1mv s3pv s1c s3sel s2c
permutation s1both s3pv s1c s3sel s2c
permutation s1uu s4m s1c s4sel s2c
permutation s1ub s4m s1c s4sel s2c
permutation s1ud s4m s1c s4sel s2c
permutation s1um s4m s1c s4sel s2c
permutation s1uu s5h s1c s4sel s2c
permutation s1ub s5h s1c s4sel s2c
permutation s1ud s5h s1c s4sel s2c
permutation s1um s5h s1c s4sel s2c
permutation s1uu s6u s1c s4sel s2c
permutation s1ub s6u s1c s4sel s2c
permutation s1ud s6u s1c s4sel s2c
permutation s1um s6u s1c s4sel s2c
permutation s1uu s7f s1c s4sel s2c
permutation s1ud s7f s1c s4sel s2c
permutation s1u15 s7f s1c s4sel s2c
permutation s1d15 s7f s1c s4sel s2c
permutation s1k s7f s1c s4sel s2c
permutation s3ve s1upd s3vm s1c s3sel s2c
permutation s1del s3vm s1c s3sel s2c
permutation s1mv s3vm s1c s3sel s2c
permutation s1upd s3vm s1a s3sel s2c

# mergecmd D655 Row C, merge#304 (merge/epq.rs SORTED and BUILT over a CTE source): regress merge#304's two data-modifying
# CTE MERGEs, `WITH m AS (MERGE INTO st t USING ss s ON tid = sid ..), m2 AS (MERGE INTO sl l USING m ON l.tid = m.tid ..)
# SELECT * FROM m2`, under default statistics (no ANALYZE, as the regress file): m is a Merge Left Join (SORTED: the source
# sorted, the target the sorted inner), m2 a Hash Left Join over the CTE's rows with the log hashed (BUILT, its source m's
# rows under a COPY mark); EXPLAIN pins both. s2's MERGE waits on a row s1 changed; after s1 commits, the redo puts the
# newest version in the write site, reruns the join over it and runs the WHEN MATCHED list again from its first action.
# SORTED (st): another column changed keeps the row matched (the UPDATE over the newest version); a changed join key makes
# it NOT MATCHED (the INSERT of the source row); a deleted row too (TM_Deleted); a flipped first WHEN condition runs the
# first of two MATCHED actions instead of the second (s2q); a MATCHED-only MERGE, an inner merge join (its source key
# carried by `tid = sid`), does nothing for a re-keyed row (s2i); s1 rolled back writes the old version. BUILT over the CTE (sl): the log's row changed keeps it
# matched; re-keyed or deleted, the CTE's row inserts (NOT MATCHED). D3379: a SORTED MERGE with a join qual past the merge
# clause (s2j, `ON tid = sid AND t.balance > s.delta`) rechecks the qual over the newest version: still true keeps the row
# matched (s1u), false makes it NOT MATCHED (s1jq: the INSERT of the source row). A MATCHED-only MERGE whose actions never
# read the source key (s2h: no `tid = sid` SET, so the join's projection does not carry sid) rechecks the merge clause
# over the source row the join read: a changed balance keeps the row matched (s1u), a re-keyed row does nothing (s1k).

setup
{
  CREATE TABLE st (tid integer NOT NULL, balance integer) WITH (autovacuum_enabled = off);
  CREATE TABLE ss (delta integer, sid integer, balance integer DEFAULT 0) WITH (autovacuum_enabled = off);
  INSERT INTO st (tid, balance) VALUES (1, 100), (2, 200), (3, 300);
  INSERT INTO ss (sid, delta) VALUES (1, 10), (2, 20), (4, 40);
  CREATE TABLE sl (tid integer NOT NULL, last_change text);
  INSERT INTO sl VALUES (1, 'Original value');
}

teardown
{
  DROP TABLE st, ss, sl;
}

session s1
setup		{ BEGIN; }
step s1u	{ UPDATE st SET balance = balance + 5 WHERE tid = 2; }
step s1k	{ UPDATE st SET tid = 102 WHERE tid = 2; }
step s1d	{ DELETE FROM st WHERE tid = 1; }
step s1q	{ UPDATE st SET balance = 999 WHERE tid = 2; }
step s1jq	{ UPDATE st SET balance = 5 WHERE tid = 2; }
step s1lu	{ UPDATE sl SET last_change = 's1' WHERE tid = 1; }
step s1lk	{ UPDATE sl SET tid = 101 WHERE tid = 1; }
step s1ld	{ DELETE FROM sl WHERE tid = 1; }
step s1c	{ COMMIT; }
step s1r	{ ROLLBACK; }

session s2
step s2x	{ EXPLAIN (COSTS OFF) WITH m AS (MERGE INTO st t USING ss s ON tid = sid WHEN MATCHED AND tid >= 2 THEN UPDATE SET balance = t.balance + delta WHEN NOT MATCHED THEN INSERT (balance, tid) VALUES (balance + delta, sid) WHEN MATCHED AND tid < 2 THEN DELETE RETURNING merge_action() AS action, t.*, CASE merge_action() WHEN 'INSERT' THEN 'Inserted ' || t WHEN 'UPDATE' THEN 'Added ' || delta || ' to balance' WHEN 'DELETE' THEN 'Removed ' || t END AS description), m2 AS (MERGE INTO sl l USING m ON l.tid = m.tid WHEN MATCHED THEN UPDATE SET last_change = description WHEN NOT MATCHED THEN INSERT VALUES (m.tid, description) RETURNING m.*, merge_action() AS log_action, l.*) SELECT * FROM m2; }
step s2xq	{ EXPLAIN (COSTS OFF) WITH m AS (MERGE INTO st t USING ss s ON tid = sid WHEN MATCHED AND t.balance > 500 THEN DELETE WHEN MATCHED THEN UPDATE SET balance = t.balance + delta WHEN NOT MATCHED THEN INSERT (balance, tid) VALUES (balance + delta, sid) RETURNING merge_action() AS act, t.*) SELECT * FROM m; }
step s2xi	{ EXPLAIN (COSTS OFF) WITH m AS (MERGE INTO st t USING ss s ON tid = sid WHEN MATCHED THEN UPDATE SET balance = t.balance + delta, tid = sid RETURNING t.*) SELECT * FROM m; }
step s2xj	{ EXPLAIN (COSTS OFF) WITH m AS (MERGE INTO st t USING ss s ON tid = sid AND t.balance > s.delta WHEN MATCHED THEN UPDATE SET balance = t.balance + delta WHEN NOT MATCHED THEN INSERT (balance, tid) VALUES (balance + delta, sid) RETURNING merge_action() AS act, t.*) SELECT * FROM m ORDER BY tid, act; }
step s2xh	{ EXPLAIN (COSTS OFF) WITH m AS (MERGE INTO st t USING ss s ON tid = sid WHEN MATCHED THEN UPDATE SET balance = t.balance + delta RETURNING t.*) SELECT * FROM m ORDER BY tid; }
step s2		{ WITH m AS (MERGE INTO st t USING ss s ON tid = sid WHEN MATCHED AND tid >= 2 THEN UPDATE SET balance = t.balance + delta WHEN NOT MATCHED THEN INSERT (balance, tid) VALUES (balance + delta, sid) WHEN MATCHED AND tid < 2 THEN DELETE RETURNING merge_action() AS action, t.*, CASE merge_action() WHEN 'INSERT' THEN 'Inserted ' || t WHEN 'UPDATE' THEN 'Added ' || delta || ' to balance' WHEN 'DELETE' THEN 'Removed ' || t END AS description), m2 AS (MERGE INTO sl l USING m ON l.tid = m.tid WHEN MATCHED THEN UPDATE SET last_change = description WHEN NOT MATCHED THEN INSERT VALUES (m.tid, description) RETURNING m.*, merge_action() AS log_action, l.*) SELECT * FROM m2 ORDER BY 2; }
step s2q	{ WITH m AS (MERGE INTO st t USING ss s ON tid = sid WHEN MATCHED AND t.balance > 500 THEN DELETE WHEN MATCHED THEN UPDATE SET balance = t.balance + delta WHEN NOT MATCHED THEN INSERT (balance, tid) VALUES (balance + delta, sid) RETURNING merge_action() AS act, t.*) SELECT * FROM m ORDER BY tid; }
step s2j	{ WITH m AS (MERGE INTO st t USING ss s ON tid = sid AND t.balance > s.delta WHEN MATCHED THEN UPDATE SET balance = t.balance + delta WHEN NOT MATCHED THEN INSERT (balance, tid) VALUES (balance + delta, sid) RETURNING merge_action() AS act, t.*) SELECT * FROM m ORDER BY tid, act; }
step s2i	{ WITH m AS (MERGE INTO st t USING ss s ON tid = sid WHEN MATCHED THEN UPDATE SET balance = t.balance + delta, tid = sid RETURNING t.*) SELECT * FROM m ORDER BY tid; }
step s2h	{ WITH m AS (MERGE INTO st t USING ss s ON tid = sid WHEN MATCHED THEN UPDATE SET balance = t.balance + delta RETURNING t.*) SELECT * FROM m ORDER BY tid; }

session s3
step s3		{ SELECT tid, balance FROM st ORDER BY tid; }
step s3l	{ SELECT tid, last_change FROM sl ORDER BY tid; }

permutation s2x s2xq s2xi s1u s2 s1c s3 s3l
permutation s1k s2 s1c s3 s3l
permutation s1d s2 s1c s3 s3l
permutation s1q s2q s1c s3
permutation s1k s2i s1c s3
permutation s1u s1k s2 s1r s3 s3l
permutation s1lu s2 s1c s3l
permutation s1lk s2 s1c s3l
permutation s1ld s2 s1c s3l
permutation s2xj s1u s2j s1c s3
permutation s1jq s2j s1c s3
permutation s2xh s1u s2h s1c s3
permutation s1k s2h s1c s3

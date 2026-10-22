# mergecmd D1933 (b) (merge#241, publication#984): a MERGE over a nested loop whose inner is a Materialize, while another
# session changes the matched target row (READ COMMITTED). PostgreSQL 18.6's EvalPlanQual runs the plan again in a fresh
# planstate tree: the target's scan returns the newest version alone, the source's REFERENCE row is refetched at its
# ctid (the version the join read), and the Materialize refills once.
#  (b) the source outer, the target under the Material (MATCHED UPDATE, NOT MATCHED INSERT):
#   (1) s1 updates the matched row: the UPDATE runs over its newest version
#   (2) s1 deletes it: the source row takes the NOT MATCHED INSERT
#   (3) s1 moves its key: the join qual fails on the newest version, the source row takes the NOT MATCHED INSERT
#   (4) s1 aborts: the original version is updated (control)
#  (a) the target outer, the source under the Material (MATCHED UPDATE / DELETE):
#   (5) s1 updates the matched row: the action runs over its newest version
#   (6) s1 updates the source row the join read: the recheck reads the version the join read
#   (7) s1 moves the target key: the join qual fails on the newest version: nothing done
#   (8) NOT MATCHED BY SOURCE over the LEFT nested loop: s1 updates an unmatched row: the action runs over its newest
#       version (the source position empty: the Material refills nothing)
# Expected output is PostgreSQL 18.6's (the C oracle).
setup
{
 CREATE TABLE dr_t (tid int, bal int, val text);
 CREATE TABLE dr_s (sid int, delta int);
 CREATE TABLE dr_big (tid int, bal int);
 CREATE TABLE dr_s3 (sid int, delta int);
 INSERT INTO dr_t VALUES (1, 100, 'a'), (2, 200, 'b'), (3, 300, 'c');
 INSERT INTO dr_s SELECT g, g * 10 FROM generate_series(2, 40) g;
 INSERT INTO dr_big SELECT g, g * 100 FROM generate_series(1, 60) g;
 INSERT INTO dr_s3 VALUES (2, 20), (3, 30), (70, 700);
 ANALYZE dr_t, dr_s, dr_big, dr_s3;
}

teardown
{
 DROP TABLE dr_t, dr_s, dr_big, dr_s3;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1upd	{ UPDATE dr_t SET bal = bal + 1 WHERE tid = 2; }
step s1del	{ DELETE FROM dr_t WHERE tid = 2; }
step s1key	{ UPDATE dr_t SET tid = 50 WHERE tid = 2; }
step s1bupd	{ UPDATE dr_big SET bal = bal + 1 WHERE tid = 3; }
step s1supd	{ UPDATE dr_s3 SET delta = 99 WHERE sid = 3; }
step s1bkey	{ UPDATE dr_big SET tid = 90 WHERE tid = 3; }
step s1b1	{ UPDATE dr_big SET bal = bal + 5 WHERE tid = 1; }
step s1c	{ COMMIT; }
step s1a	{ ROLLBACK; }

session s2
step s2expb	{ EXPLAIN (COSTS OFF) MERGE INTO dr_t t USING dr_s s ON t.tid IS NOT DISTINCT FROM s.sid WHEN MATCHED THEN UPDATE SET bal = t.bal + s.delta WHEN NOT MATCHED AND s.sid < 4 THEN INSERT VALUES (s.sid, s.delta, 'new'); }
step s2b	{ MERGE INTO dr_t t USING dr_s s ON t.tid IS NOT DISTINCT FROM s.sid WHEN MATCHED THEN UPDATE SET bal = t.bal + s.delta WHEN NOT MATCHED AND s.sid < 4 THEN INSERT VALUES (s.sid, s.delta, 'new'); }
step s2sel	{ SELECT * FROM dr_t ORDER BY tid, bal; }
step s2expa	{ EXPLAIN (COSTS OFF) MERGE INTO dr_big t USING dr_s3 s ON t.tid IS NOT DISTINCT FROM s.sid WHEN MATCHED AND s.sid > 2 THEN UPDATE SET bal = t.bal + s.delta WHEN MATCHED THEN DELETE; }
step s2a	{ MERGE INTO dr_big t USING dr_s3 s ON t.tid IS NOT DISTINCT FROM s.sid WHEN MATCHED AND s.sid > 2 THEN UPDATE SET bal = t.bal + s.delta WHEN MATCHED THEN DELETE; }
step s2bs	{ MERGE INTO dr_big t USING dr_s3 s ON t.tid IS NOT DISTINCT FROM s.sid WHEN MATCHED THEN UPDATE SET bal = s.delta WHEN NOT MATCHED BY SOURCE AND t.tid < 2 THEN UPDATE SET bal = t.bal * 2; }
step s2bsel	{ SELECT * FROM dr_big WHERE tid < 5 OR tid > 60 ORDER BY tid; }

permutation s2expb s1upd s2b s1c s2sel
permutation s1del s2b s1c s2sel
permutation s1key s2b s1c s2sel
permutation s1upd s2b s1a s2sel
permutation s2expa s1bupd s2a s1c s2bsel
permutation s1supd s2a s1c s2bsel
permutation s1bkey s2a s1c s2bsel
permutation s1b1 s2bs s1c s2bsel

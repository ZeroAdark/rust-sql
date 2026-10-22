# D1933 (b): a MERGE over a nested loop whose inner is a Materialize over a bitmap heap scan of the source (the target's
# own scan the outer), while another session changes the matched target row (READ COMMITTED). PostgreSQL 18.6's
# EvalPlanQual runs the plan again in a fresh planstate tree: the target's scan returns the newest version alone, the
# source's REFERENCE row is refetched at its ctid (the version the join read) and handed through the bitmap scan's
# recheck quals, and the Materialize refills once.
#   (1) s1 updates the matched row: the UPDATE action runs over its newest version
#   (2) s1 updates the row the DELETE action takes: the DELETE runs over its newest version
#   (3) s1 moves the target key: the join qual fails on the newest version: nothing done for it
#   (4) s1 deletes the matched row: nothing done for it
#   (5) s1 updates the source row the join read, then the target row: the recheck reads the source version the join read
#   (6) s1 aborts: the original version is updated (control)
# Expected output is PostgreSQL 18.6's (the C oracle).
setup
{
 CREATE TABLE bf_t (tid int, bal int, val text);
 CREATE TABLE bf_s (sid int PRIMARY KEY, delta int);
 INSERT INTO bf_t VALUES (1, 100, 'a'), (2, 200, 'b'), (3, 300, 'c');
 INSERT INTO bf_s SELECT g, g * 10 FROM generate_series(2, 40) g;
}

teardown
{
 DROP TABLE bf_t, bf_s;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1upd	{ UPDATE bf_t SET bal = bal + 1 WHERE tid = 3; }
step s1upd2	{ UPDATE bf_t SET bal = bal + 7 WHERE tid = 2; }
step s1key	{ UPDATE bf_t SET tid = 90 WHERE tid = 3; }
step s1del	{ DELETE FROM bf_t WHERE tid = 3; }
step s1supd	{ UPDATE bf_s SET delta = 99 WHERE sid = 3; }
step s1c	{ COMMIT; }
step s1a	{ ROLLBACK; }

session s2
step s2exp	{ EXPLAIN (COSTS OFF) MERGE INTO bf_t t USING bf_s s ON t.tid IS NOT DISTINCT FROM s.sid AND s.sid >= 2 WHEN MATCHED AND s.sid > 2 THEN UPDATE SET bal = t.bal + s.delta WHEN MATCHED THEN DELETE; }
step s2m	{ MERGE INTO bf_t t USING bf_s s ON t.tid IS NOT DISTINCT FROM s.sid AND s.sid >= 2 WHEN MATCHED AND s.sid > 2 THEN UPDATE SET bal = t.bal + s.delta WHEN MATCHED THEN DELETE; }
step s2sel	{ SELECT * FROM bf_t ORDER BY tid, bal; }

permutation s2exp s1upd s2m s1c s2sel
permutation s1upd2 s2m s1c s2sel
permutation s1key s2m s1c s2sel
permutation s1del s2m s1c s2sel
permutation s1supd s1upd s2m s1c s2sel
permutation s1upd s2m s1a s2sel

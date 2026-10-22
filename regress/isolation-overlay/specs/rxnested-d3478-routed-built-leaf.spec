# D3478 (mergecmd MOVE (d)): MERGE through a partitioned root the planner prunes to one leaf, the leaf's rows a hash
# build the source probes (nested loops and merge joins off), its MATCHED UPDATE moving the row to another leaf, while
# another session changes the matched row (READ COMMITTED). PostgreSQL 18.6's ExecMergeMatched rechecks the row under
# EvalPlanQual: the source's REFERENCE mark hands back the row it read, the leaf's newest version is joined to it, and the
# action list runs again over that version (ExecCrossPartitionUpdate leaves the recheck to ExecMergeMatched).
# The EXPLAIN step pins the plan (the leaf hashed, the source probing).
#  (a) s1 updates the matched row in place: the move runs over the newest version (its bal)
#  (b) s1 changes the matched row's key within its leaf: the original source row no longer joins, so no action
#  (c) s1 deletes the matched row: NOT MATCHED, no action
#  (d) s1 moves the matched row to another leaf: the serialization error of a row moved by a concurrent update
#  (e) s1 updates a source row: the MERGE reads the source row it saw and does not wait
# Expected output is PostgreSQL 18.6's (the C oracle).
setup
{
 CREATE TABLE hb_t (tid int, bal int, val text) PARTITION BY LIST (tid);
 CREATE TABLE hb_t1 PARTITION OF hb_t FOR VALUES IN (1, 4);
 CREATE TABLE hb_t2 PARTITION OF hb_t FOR VALUES IN (2, 5, 6);
 CREATE TABLE hb_t3 PARTITION OF hb_t FOR VALUES IN (3, 7, 8);
 CREATE TABLE hb_s (sid int, delta int);
 INSERT INTO hb_t SELECT g, g * 100, 'v' || g FROM generate_series(1, 8) g;
 INSERT INTO hb_s SELECT g, g * 10 FROM generate_series(2, 8, 2) g;
 ANALYZE hb_t, hb_s;
}

teardown
{
 DROP TABLE hb_t, hb_s;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1bal	{ UPDATE hb_t SET bal = bal + 1 WHERE tid = 2; }
step s1key	{ UPDATE hb_t SET tid = 5 WHERE tid = 2; }
step s1del	{ DELETE FROM hb_t WHERE tid = 2; }
step s1move	{ UPDATE hb_t SET tid = 4 WHERE tid = 2; }
step s1src	{ UPDATE hb_s SET delta = 999 WHERE sid = 2; }
step s1c	{ COMMIT; }

session s2
setup		{ SET enable_nestloop = off; SET enable_mergejoin = off; }
step s2exp	{ EXPLAIN (COSTS OFF) MERGE INTO hb_t t USING hb_s s ON t.tid = s.sid AND t.tid IN (2, 6) WHEN MATCHED THEN UPDATE SET tid = tid + 1, bal = bal + delta, val = val || ' moved'; }
step s2mrg	{ MERGE INTO hb_t t USING hb_s s ON t.tid = s.sid AND t.tid IN (2, 6) WHEN MATCHED THEN UPDATE SET tid = tid + 1, bal = bal + delta, val = val || ' moved'; }
step s2sel	{ SELECT tableoid::regclass, * FROM hb_t ORDER BY tid, val; }

permutation s2exp s1bal s2mrg s1c s2sel
permutation s1key s2mrg s1c s2sel
permutation s1del s2mrg s1c s2sel
permutation s1move s2mrg s1c s2sel
permutation s1src s2mrg s1c s2sel

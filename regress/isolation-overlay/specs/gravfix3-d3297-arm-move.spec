# D3297 (regress partition_prune#424: `UPDATE ab_a1 SET b = 3 FROM ab_a2 WHERE ab_a2.b = (SELECT 1)`): an UPDATE of a
# partitioned target's leaves (three result relations, a Nested Loop over their Append) whose inner is a Materialize over
# a second partitioned table's leaves pruned at run time over an InitPlan's output, the SET moving rows to another leaf,
# while another session changes the target rows (READ COMMITTED). Each target's arm rechecks its own rows in the IR
# (lowerv4 write/nest_outer.rs, producerchain ARM): EvalPlanQual reruns the plan with the newest target version and the
# source member's row refetched at the junk ctid and tableoid the statement joined (the version the join read), the
# InitPlan's value copied down from the statement (EvalPlanQualStart). A move's delete leg meeting the newer version runs
# that recheck, then the update again: in place or a move.
#  (a) s1 updates a moving row's v, the row joined to one source row: the move runs over the newest version (its v)
#  (b) the same with the row joined to two source rows: the first pair moves the newest version, the second meets the
#      version this command moved and raises PostgreSQL's 40001 (tuple to be locked was already moved ..)
#  (c) s1 moves the row to another leaf itself: the lock finds it moved (40001)
#  (d) s1 deletes the row: skipped, the other rows move
#  (e) s1 changes the joined source row too: the recheck joins the source version the statement read
#  (f) an UPDATE that moves nothing (the SET leaves the partition key) over (a) and (e): a row joined twice is updated once
#  (g) s1 aborts: the original version moves (control)
# s2 plans with nested loops alone. Expected output is PostgreSQL 18.6's (the C oracle).
setup
{
 CREATE TABLE dt (k int, v int) PARTITION BY LIST (k);
 CREATE TABLE dt1 PARTITION OF dt FOR VALUES IN (1);
 CREATE TABLE dt2 PARTITION OF dt FOR VALUES IN (2);
 CREATE TABLE dt3 PARTITION OF dt FOR VALUES IN (3);
 INSERT INTO dt VALUES (1, 0), (2, 0), (3, 0);
 CREATE TABLE ds (a int, b int) PARTITION BY LIST (b);
 CREATE TABLE ds1 PARTITION OF ds FOR VALUES IN (1);
 CREATE TABLE ds2 PARTITION OF ds FOR VALUES IN (2);
 CREATE TABLE ds3 PARTITION OF ds FOR VALUES IN (3);
 INSERT INTO ds VALUES (10, 1), (20, 1), (30, 2), (40, 3);
}

teardown
{
 DROP TABLE dt, ds;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1u	{ UPDATE dt SET v = v + 100 WHERE k = 1; }
step s1k	{ UPDATE dt SET k = 2 WHERE k = 1; }
step s1d	{ DELETE FROM dt WHERE k = 1; }
step s1s	{ UPDATE ds SET a = a + 1000 WHERE b = 1; }
step s1c	{ COMMIT; }
step s1a	{ ROLLBACK; }

session s2
setup		{ SET enable_hashjoin = off; SET enable_mergejoin = off; SET max_parallel_workers_per_gather = 0; SET plan_cache_mode = force_generic_plan; BEGIN ISOLATION LEVEL READ COMMITTED; }
step s2m	{ UPDATE dt SET k = 3, v = dt.v + ds.a FROM ds WHERE ds.b = (SELECT 1); }
step s2m1	{ UPDATE dt SET k = 3, v = dt.v + ds.a FROM ds WHERE ds.b = (SELECT 1) AND ds.a < 15; }
step s2v	{ UPDATE dt SET v = dt.v + ds.a FROM ds WHERE ds.b = (SELECT 1); }
step s2c	{ COMMIT; }

session s3
step s3r	{ SELECT tableoid::regclass, * FROM dt ORDER BY k, v; }

permutation s1u s2m1 s1c s2c s3r
permutation s1u s2m s1c s2c s3r
permutation s1k s2m1 s1c s2c s3r
permutation s1d s2m s1c s2c s3r
permutation s1u s1s s2m1 s1c s2c s3r
permutation s1u s1s s2v s1c s2c s3r
permutation s1u s2v s1c s2c s3r
permutation s1u s2m1 s1a s2c s3r

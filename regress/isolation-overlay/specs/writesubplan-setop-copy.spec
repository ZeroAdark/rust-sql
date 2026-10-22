# writesubplan (gen setop_core-0157 / 0158): a DELETE / UPDATE .. WHERE k IN (SELECT .. EXCEPT / INTERSECT ..) planned
# with the Hash on the target and the subquery the probe (Hash Join, Subquery Scan "ANY_subquery" over a HashSetOp), while
# another session changes a target row (READ COMMITTED). PostgreSQL 18.6's EvalPlanQual refetches the newest version and
# hands the Subquery Scan its ROW_MARK_COPY row (the set operation does not run again), so the join qual is rechecked
# against the stored subquery row only:
#  (a) s1 changes a non-key column of row 1: still deleted
#  (b) s1 moves row 1's key out of the set: its recheck fails, the row stays
#  (c) s1 moves row 1's key to 2, another key of the set: the recheck compares it with the stored row's 1 and fails, the
#      row stays (no other subquery row is tried)
#  (d) s1 deletes row 1: nothing to delete there
#  (e) s1 aborts: the original versions (control)
#  (f) (a) and (b) with RETURNING of row 1 alone
#  (g) the UPDATE .. INTERSECT over row 3: a non-key change (the update reads the newest version) and a key move
# Each permutation ends s2's transaction. Expected output is PostgreSQL 18.6's (the C oracle).
setup
{
 CREATE TABLE wsc_sink (id int, k int, t text);
 INSERT INTO wsc_sink SELECT i, i % 997, 'sink' || i FROM generate_series(1, 50) i;
 CREATE TABLE wsc_tb (id int, g int, k int);
 INSERT INTO wsc_tb SELECT i, i % 7, i % 1000 FROM generate_series(1, 20000) i;
 CREATE TABLE wsc_tsm (k int);
 INSERT INTO wsc_tsm SELECT i * 3 FROM generate_series(1, 300) i;
 ANALYZE wsc_sink, wsc_tb, wsc_tsm;
}

teardown
{
 DROP TABLE wsc_sink, wsc_tb, wsc_tsm;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1t	{ UPDATE wsc_sink SET t = 'x' WHERE id = 1; }
step s1k	{ UPDATE wsc_sink SET k = 9999 WHERE id = 1; }
step s1k2	{ UPDATE wsc_sink SET k = 2 WHERE id = 1; }
step s1d	{ DELETE FROM wsc_sink WHERE id = 1; }
step s1t3	{ UPDATE wsc_sink SET t = 'y' WHERE id = 3; }
step s1k3	{ UPDATE wsc_sink SET k = 9999 WHERE id = 3; }
step s1c	{ COMMIT; }
step s1a	{ ROLLBACK; }

session s2
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s2e	{ EXPLAIN (COSTS OFF) DELETE FROM wsc_sink WHERE k IN (SELECT k FROM wsc_tb WHERE g = 0 EXCEPT SELECT k FROM wsc_tsm); }
step s2d	{ DELETE FROM wsc_sink WHERE k IN (SELECT k FROM wsc_tb WHERE g = 0 EXCEPT SELECT k FROM wsc_tsm); }
step s2r	{ DELETE FROM wsc_sink WHERE id = 1 AND k IN (SELECT k FROM wsc_tb WHERE g = 0 EXCEPT SELECT k FROM wsc_tsm) RETURNING id, k, t; }
step s2ue	{ EXPLAIN (COSTS OFF) UPDATE wsc_sink SET t = t || '+both' WHERE k IN (SELECT k FROM wsc_tb INTERSECT SELECT k FROM wsc_tsm); }
step s2u	{ UPDATE wsc_sink SET t = t || '+both' WHERE k IN (SELECT k FROM wsc_tb INTERSECT SELECT k FROM wsc_tsm); }
step s2sel	{ SELECT id, k, t FROM wsc_sink WHERE id IN (1, 2, 3, 6) ORDER BY id; }
step s2c	{ COMMIT; }

permutation s2e s1t s2d s1c s2sel s2c
permutation s1k s2d s1c s2sel s2c
permutation s1k2 s2d s1c s2sel s2c
permutation s1d s2d s1c s2sel s2c
permutation s1t s2d s1a s2sel s2c
permutation s1t s2r s1c s2sel s2c
permutation s1k s2r s1c s2sel s2c
permutation s2ue s1t3 s2u s1c s2sel s2c
permutation s1k3 s2u s1c s2sel s2c

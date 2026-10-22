# D2735 COPY-EMIT (modifytable, core-0274's form): s2's DELETE / UPDATE ... WHERE cid IN (SELECT .. INTERSECT SELECT ..)
# is a Hash Join probing a Hash over the Subquery Scan of a HashSetOp, the subquery COPY-marked. s1 changes target rows
# (and the subquery's inputs) and commits while s2 waits. EvalPlanQual rechecks the target's newest version against the
# stored subquery row and never runs the set operation again: a row whose other column s1 changed is written as it is
# now, one whose cid s1 moved to another value (even one inside the set) or that s1 deleted is skipped, and a row whose
# subquery input s1 changed still matches its stored row. With s1 rolled back, s2 writes the versions it saw.

setup
{
 CREATE TABLE ct_c (id int PRIMARY KEY, tier int);
 CREATE TABLE ct_o (cid int, status text);
 CREATE TABLE ct_t (id int PRIMARY KEY, cid int, note text);
 INSERT INTO ct_c SELECT i, i % 2 FROM generate_series(1, 10) i;
 INSERT INTO ct_o SELECT i, 'returned' FROM generate_series(1, 10) i;
 INSERT INTO ct_t SELECT i, i, 'n' || i FROM generate_series(1, 10) i;
 ANALYZE ct_c, ct_o, ct_t;
}

teardown
{
 DROP TABLE ct_t, ct_o, ct_c;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1n	{ UPDATE ct_t SET note = note || '*' WHERE id IN (1, 3); }
step s1k	{ UPDATE ct_t SET cid = 4 WHERE id = 5; }
step s1x	{ DELETE FROM ct_t WHERE id = 7; }
step s1o	{ UPDATE ct_t SET cid = 3 WHERE id = 9; }
step s1s	{ UPDATE ct_c SET tier = 0 WHERE id = 3; UPDATE ct_t SET note = note || '#' WHERE id = 3; }
step s1c	{ COMMIT; }
step s1r	{ ROLLBACK; }

session s2
setup		{ SET enable_nestloop = off; SET enable_mergejoin = off; BEGIN ISOLATION LEVEL READ COMMITTED; }
step s2d	{ DELETE FROM ct_t WHERE cid IN (SELECT id FROM ct_c WHERE tier = 1 INTERSECT SELECT cid FROM ct_o WHERE status = 'returned') RETURNING id, cid, note; }
step s2u	{ UPDATE ct_t SET note = note || '+' WHERE cid IN (SELECT id FROM ct_c WHERE tier = 1 INTERSECT SELECT cid FROM ct_o WHERE status = 'returned') RETURNING id, cid, note; }
step s2c	{ COMMIT; }

session s3
step s3sel	{ SELECT id, cid, note FROM ct_t ORDER BY id; }

permutation s1n s1k s1x s1o s2d s1c s2c s3sel
permutation s1n s1k s1x s1o s2u s1c s2c s3sel
permutation s1s s2d s1c s2c s3sel
permutation s1n s1x s2d s1r s2c s3sel

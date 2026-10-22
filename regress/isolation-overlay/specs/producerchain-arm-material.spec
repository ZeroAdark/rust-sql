# producerchain ARM (rowsecurity#342 / #343, rules#431-#433): an UPDATE / DELETE of several result relations (the
# inheritance parent ap and its children ac1 / ac2: Nested Loop over their Append) whose inner is a Material over a
# second inheritance parent's members (bp, bc1, bc2) meets target rows s1 changed in more than one child. Each target's
# arm rechecks its own rows: EvalPlanQual reruns the plan with that target's newest version and each member's row
# refetched at the junk ctid and tableoid the statement joined (the other members return nothing), so s1's change to a
# member row is not seen: a version that still passes the join filter is written, one whose key moved off the joined
# row or was deleted is skipped, a DELETE's row joined twice is deleted once. The view rule's form (s2r: a Material over
# the COPY-marked view's Subquery Scan) rechecks over the stored row.

setup
{
 CREATE TABLE ap (a int, k int NOT NULL, x int NOT NULL);
 CREATE TABLE ac1 () INHERITS (ap);
 CREATE TABLE ac2 () INHERITS (ap);
 INSERT INTO ap SELECT i, i % 9, 0 FROM generate_series(1, 20) i;
 INSERT INTO ac1 SELECT i, i % 13, 0 FROM generate_series(1, 40) i;
 INSERT INTO ac2 SELECT i, i % 11, 0 FROM generate_series(1, 30) i;
 CREATE TABLE bp (a int, k int NOT NULL);
 CREATE TABLE bc1 () INHERITS (bp);
 CREATE TABLE bc2 () INHERITS (bp);
 INSERT INTO bp SELECT i, i % 9 FROM generate_series(1, 40) i;
 INSERT INTO bc1 SELECT i, i % 50 FROM generate_series(1, 100) i;
 INSERT INTO bc2 SELECT i, i % 11 FROM generate_series(1, 30) i;
 ANALYZE ap, ac1, ac2, bp, bc1, bc2;
 CREATE VIEW ap_ordered AS SELECT * FROM ap ORDER BY a;
 CREATE RULE ap_ordered_upd AS ON UPDATE TO ap_ordered DO INSTEAD UPDATE ap SET x = new.x WHERE a = old.a AND k = old.k;
}

teardown
{
 DROP VIEW ap_ordered;
 DROP TABLE ap CASCADE;
 DROP TABLE bp CASCADE;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1x	{ UPDATE ac1 SET x = x + 1000 WHERE a = 10; }
step s1k	{ UPDATE ac2 SET k = 0 WHERE a = 20; }
step s1d	{ DELETE FROM ap WHERE a = 16 AND k = 7; }
step s1s	{ UPDATE ONLY bp SET k = 0 WHERE a = 7; }
step s1v	{ UPDATE ac1 SET x = x + 5 WHERE a = 4; }
step s1c	{ COMMIT; }

session s2
setup		{ SET enable_hashjoin = off; SET enable_mergejoin = off; SET max_parallel_workers_per_gather = 0; BEGIN ISOLATION LEVEL READ COMMITTED; }
step s2u	{ UPDATE ap SET x = ap.x + bp.a FROM bp WHERE bp.a < ap.k AND bp.k = 7 AND ap.a <= 20 RETURNING ap.a, ap.k, ap.x; }
step s2d	{ DELETE FROM ap USING bp WHERE bp.a BETWEEN ap.k - 1 AND ap.k AND bp.k IN (1, 2) AND ap.x = 0 AND ap.a <= 20 RETURNING ap.a, ap.k; }
step s2r	{ UPDATE ap_ordered SET x = x + 100 WHERE a = 4 AND k = 4; }
step s2c	{ COMMIT; }

session s3
setup		{ SET enable_hashjoin = off; SET enable_mergejoin = off; SET max_parallel_workers_per_gather = 0; }
step s3sel	{ SELECT tableoid::regclass, count(*), sum(k), sum(x) FROM ap GROUP BY 1 ORDER BY 1; }
step s3row	{ SELECT tableoid::regclass, a, k, x FROM ap WHERE a IN (4, 7, 8, 10, 16, 20) ORDER BY 1, 2, 3; }
step s3pu	{ EXPLAIN (COSTS OFF) UPDATE ap SET x = ap.x + bp.a FROM bp WHERE bp.a < ap.k AND bp.k = 7 AND ap.a <= 20; }
step s3pd	{ EXPLAIN (COSTS OFF) DELETE FROM ap USING bp WHERE bp.a BETWEEN ap.k - 1 AND ap.k AND bp.k IN (1, 2) AND ap.x = 0 AND ap.a <= 20; }
step s3pr	{ EXPLAIN (COSTS OFF) UPDATE ap_ordered SET x = x + 100 WHERE a = 4 AND k = 4; }

permutation s3pu s3pd s3pr s1c s2c
permutation s1x s1k s1d s2u s1c s2c s3sel s3row
permutation s1x s1k s1d s1s s2u s1c s2c s3sel s3row
permutation s1k s1d s2d s1c s2c s3sel s3row
permutation s2u s1x s2c s1c s3sel s3row
permutation s1v s2r s1c s2c s3row

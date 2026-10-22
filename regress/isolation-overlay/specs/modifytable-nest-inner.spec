# D2739 NEST-INNER (modifytable, tour subq-0323's form): s2's data-modifying CTE DELETE ... WHERE order_id IN (SELECT id FROM
# no WHERE ..) is a Nested Loop whose outer is no's scan (REFERENCE-marked) and whose inner is ni's index scan keyed by
# no.id. s1 changes target rows and commits while s2 waits. EvalPlanQual refetches the no row the join read and reruns
# the inner scan over the target's newest version: a row whose qty s1 changed is deleted and returned as it is now, one
# whose order_id s1 moved off the joined order (even onto another joined order) or that s1 deleted is skipped, and a
# change s1 made to the no row itself does not matter (the refetched version is the one the join read). 400 orders so
# PostgreSQL 18.6 drives the join from no_o's scan with ni's index scan inside (bitmap scans off).

setup
{
 CREATE TABLE no_o (id int PRIMARY KEY, status text);
 CREATE TABLE no_i (order_id int, line int, qty int, PRIMARY KEY (order_id, line));
 INSERT INTO no_o SELECT i, CASE WHEN i <= 8 AND i % 2 = 0 THEN 'cancelled' ELSE 'open' END FROM generate_series(1, 400) i;
 INSERT INTO no_i SELECT i, l, i * 10 + l FROM generate_series(1, 400) i, generate_series(1, 2) l;
 ANALYZE no_o, no_i;
}

teardown
{
 DROP TABLE no_i, no_o;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1q	{ UPDATE no_i SET qty = qty + 1000 WHERE order_id = 2 AND line = 1; }
step s1m	{ UPDATE no_i SET order_id = 3, line = 7 WHERE order_id = 4 AND line = 1; }
step s1j	{ UPDATE no_i SET order_id = 8, line = 9 WHERE order_id = 6 AND line = 1; }
step s1x	{ DELETE FROM no_i WHERE order_id = 8 AND line = 2; }
step s1o	{ UPDATE no_o SET status = 'open' WHERE id = 2; UPDATE no_i SET qty = qty + 1 WHERE order_id = 2 AND line = 2; }
step s1c	{ COMMIT; }
step s1r	{ ROLLBACK; }

session s2
setup		{ SET enable_hashjoin = off; SET enable_mergejoin = off; SET enable_bitmapscan = off; BEGIN ISOLATION LEVEL READ COMMITTED; }
step s2d	{ WITH d AS (DELETE FROM no_i WHERE order_id IN (SELECT id FROM no_o WHERE status = 'cancelled') RETURNING order_id, line, qty) SELECT order_id, line, qty FROM d ORDER BY order_id, line; }
step s2c	{ COMMIT; }

session s3
step s3sel	{ SELECT order_id, line, qty FROM no_i WHERE order_id <= 9 ORDER BY order_id, line; }

permutation s1q s1m s1j s1x s2d s1c s2c s3sel
permutation s1o s2d s1c s2c s3sel
permutation s1q s1x s2d s1r s2c s3sel

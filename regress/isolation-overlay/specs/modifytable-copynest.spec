# COPYNEST (modifytable): an UPDATE / DELETE whose target is the parameterized inner of a nested loop over a subquery row
# (ROW_MARK_COPY: a grouped, ordered, limited IN subquery pulled up into a Subquery Scan) meets a target row s1 changed.
# EvalPlanQual hands the Subquery Scan the row it stored and the target's index scan the newest version, its key checked
# again against that row: a row that still matches is written over its newest version, one s1 moved off the key or
# deleted is skipped, and s1's change to the subquery's input is not seen (the subquery never runs again).

setup
{
 CREATE TABLE cnn_c (id int PRIMARY KEY, tier int, note text);
 INSERT INTO cnn_c SELECT i, 0, 'c' || i FROM generate_series(1, 300) i;
 CREATE TABLE cnn_o (id int PRIMARY KEY, cid int, amt int);
 INSERT INTO cnn_o SELECT i, i % 300 + 1, CASE WHEN i % 300 + 1 BETWEEN 11 AND 16 THEN 1000 + i ELSE i % 50 END FROM generate_series(1, 1200) i;
 ANALYZE cnn_c, cnn_o;
}

teardown
{
 DROP TABLE cnn_o;
 DROP TABLE cnn_c;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1u	{ UPDATE cnn_c SET note = note || '*' WHERE id IN (11, 12); }
step s1k	{ UPDATE cnn_c SET id = id + 1000 WHERE id = 13; }
step s1d	{ DELETE FROM cnn_c WHERE id = 14; }
step s1o	{ UPDATE cnn_o SET amt = 0 WHERE cid = 15; UPDATE cnn_c SET note = note || '+' WHERE id = 15; }
step s1c	{ COMMIT; }

session s2
setup		{ SET enable_hashjoin = off; SET enable_mergejoin = off; BEGIN ISOLATION LEVEL READ COMMITTED; }
step s2u	{ UPDATE cnn_c SET tier = 4 WHERE id IN (SELECT cid FROM cnn_o GROUP BY cid ORDER BY sum(amt) DESC, cid LIMIT 6) RETURNING id, note; }
step s2d	{ DELETE FROM cnn_c WHERE id IN (SELECT cid FROM cnn_o GROUP BY cid ORDER BY sum(amt) DESC, cid LIMIT 6); }
step s2c	{ COMMIT; }

session s3
setup		{ SET enable_hashjoin = off; SET enable_mergejoin = off; }
step s3s	{ SELECT id, tier, note FROM cnn_c WHERE id BETWEEN 10 AND 20 OR id > 1000 ORDER BY id; }
step s3e	{ EXPLAIN (COSTS OFF) UPDATE cnn_c SET tier = 4 WHERE id IN (SELECT cid FROM cnn_o GROUP BY cid ORDER BY sum(amt) DESC, cid LIMIT 6) RETURNING id, note; }

permutation s3e s1u s1k s1d s1o s2u s1c s2c s3s
permutation s1u s1k s1d s1o s2d s1c s2c s3s

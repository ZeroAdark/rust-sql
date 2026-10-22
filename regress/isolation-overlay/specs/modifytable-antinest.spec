# ANTINEST (modifytable): an UPDATE / DELETE over a Nested Loop Anti Join (NOT EXISTS over an index scan parameterized
# by the target row) meets a target row s1 changed. The anti join's rows carry the inner relation's junk ctid NULL, so
# PostgreSQL's EvalPlanQual gives that relation no test tuple and only the target's own quals are checked again over the
# newest version: a row that still qualifies is written (even when s1 also inserted a matching inner row: the recheck
# never probes the inner again), one s1 moved out of the target's qual or deleted is skipped.

setup
{
 CREATE TABLE ant_c (id int PRIMARY KEY, tier int, note text);
 INSERT INTO ant_c SELECT i, i % 3, 'c' || i FROM generate_series(1, 400) i;
 CREATE TABLE ant_o (id int PRIMARY KEY, cid int);
 INSERT INTO ant_o SELECT i, i FROM generate_series(1, 380) i;
 CREATE INDEX ant_o_cid ON ant_o (cid);
 ANALYZE ant_c, ant_o;
}

teardown
{
 DROP TABLE ant_o;
 DROP TABLE ant_c;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1u	{ UPDATE ant_c SET note = note || '*' WHERE id IN (391, 392); }
step s1q	{ UPDATE ant_c SET tier = 9 WHERE id = 393; }
step s1d	{ DELETE FROM ant_c WHERE id = 394; }
step s1o	{ INSERT INTO ant_o VALUES (1000, 395); UPDATE ant_c SET note = note || '+' WHERE id = 395; }
step s1c	{ COMMIT; }

session s2
setup		{ SET enable_hashjoin = off; SET enable_mergejoin = off; BEGIN ISOLATION LEVEL READ COMMITTED; }
step s2d	{ DELETE FROM ant_c c WHERE NOT EXISTS (SELECT 1 FROM ant_o o WHERE o.cid = c.id) AND c.id > 385 AND c.tier < 5; }
step s2u	{ UPDATE ant_c c SET note = note || '#' WHERE NOT EXISTS (SELECT 1 FROM ant_o o WHERE o.cid = c.id) AND c.id > 385 AND c.tier < 5; }
step s2c	{ COMMIT; }

session s3
setup		{ SET enable_hashjoin = off; SET enable_mergejoin = off; }
step s3s	{ SELECT id, tier, note FROM ant_c WHERE id > 385 ORDER BY id; }
step s3e	{ EXPLAIN (COSTS OFF) DELETE FROM ant_c c WHERE NOT EXISTS (SELECT 1 FROM ant_o o WHERE o.cid = c.id) AND c.id > 385 AND c.tier < 5; }

permutation s3e s1u s1q s1d s1o s2d s1c s2c s3s
permutation s1u s1q s1d s1o s2u s1c s2c s3s

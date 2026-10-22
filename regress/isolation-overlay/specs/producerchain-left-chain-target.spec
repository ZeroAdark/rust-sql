# producerchain G2: an UPDATE / DELETE whose target's scan is the inner of an INNER nested loop after a LEFT nested loop
# over two other relations' plain heap scans (`UPDATE lt ... FROM la LEFT JOIN lb ...`: la's scan the outer, lb's the LEFT
# nest's inner, then lt's index scan the target nest's inner), meets a target row s1 changed. EvalPlanQual reruns the
# plan with the target's newest version and the other relations' rows refetched at their junk ctids: lb's NULL ctid (an
# la row with no lb match) gives no row, so the LEFT nest null-extends again. A version that still joins is written (SET
# over its new values, lb's columns as the statement read them, or NULL), one whose key moved off the joined la row or
# that was deleted is skipped, and s1's changes to la and lb (a new matching lb row included) are not seen.

setup
{
 CREATE TABLE la (id int PRIMARY KEY, w int NOT NULL);
 INSERT INTO la SELECT i, i % 3 FROM generate_series(1, 9) i;
 CREATE TABLE lb (id int PRIMARY KEY, la_id int NOT NULL, tag text NOT NULL);
 INSERT INTO lb VALUES (1, 1, 'b1'), (4, 4, 'b4'), (8, 2, 'b8');
 CREATE TABLE lt (id int PRIMARY KEY, la_id int NOT NULL, x int NOT NULL);
 INSERT INTO lt SELECT i, 1 + i % 9, 0 FROM generate_series(1, 900) i;
 CREATE INDEX lt_la ON lt (la_id);
 ANALYZE la, lb, lt;
}

teardown
{
 DROP TABLE lt;
 DROP TABLE lb;
 DROP TABLE la;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1x	{ UPDATE lt SET x = x + 1000 WHERE id IN (9, 12, 33); }
step s1k	{ UPDATE lt SET la_id = 2 WHERE id = 18; }
step s1d	{ DELETE FROM lt WHERE id = 27; }
step s1b	{ UPDATE lb SET tag = 'b4new' WHERE id = 4; }
step s1n	{ INSERT INTO lb VALUES (10, 7, 'b10'); }
step s1a	{ UPDATE la SET w = 0 WHERE id = 7; }
step s1c	{ COMMIT; }

session s2
setup		{ SET enable_hashjoin = off; SET enable_mergejoin = off; SET enable_material = off; SET enable_memoize = off; SET enable_bitmapscan = off; SET max_parallel_workers_per_gather = 0; BEGIN ISOLATION LEVEL READ COMMITTED; }
step s2u	{ UPDATE lt SET x = x + 1 FROM la LEFT JOIN lb ON lb.la_id = la.id WHERE lt.la_id = la.id AND la.w = 1 AND lt.id < 60 RETURNING lt.id, lt.la_id, lt.x, lb.tag, la.id; }
step s2d	{ DELETE FROM lt USING la LEFT JOIN lb ON lb.la_id = la.id WHERE lt.la_id = la.id AND la.w = 1 AND lt.id < 60 RETURNING lt.id, lb.tag, la.id; }
step s2c	{ COMMIT; }

session s3
setup		{ SET enable_hashjoin = off; SET enable_mergejoin = off; SET enable_material = off; SET enable_memoize = off; SET enable_bitmapscan = off; SET max_parallel_workers_per_gather = 0; }
step s3sel	{ SELECT count(*), sum(la_id), sum(x) FROM lt; }
step s3row	{ SELECT id, la_id, x FROM lt WHERE id IN (9, 12, 33, 18, 27, 1, 3) ORDER BY id; }
step s3pu	{ EXPLAIN (COSTS OFF) UPDATE lt SET x = x + 1 FROM la LEFT JOIN lb ON lb.la_id = la.id WHERE lt.la_id = la.id AND la.w = 1 AND lt.id < 60; }
step s3pd	{ EXPLAIN (COSTS OFF) DELETE FROM lt USING la LEFT JOIN lb ON lb.la_id = la.id WHERE lt.la_id = la.id AND la.w = 1 AND lt.id < 60; }

permutation s3pu s3pd s1c s2c
permutation s1x s1k s1d s1b s1n s1a s2u s1c s2c s3sel s3row
permutation s1x s1k s1d s1b s2d s1c s2c s3sel s3row
permutation s2u s1x s2c s1c s3sel s3row

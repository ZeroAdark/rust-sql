# D2748 ROWC-UNION-UPDATE (modifytable; regress triggers#500): a data-modifying CTE's UPDATE through a partitioned root
# (several result relations, no RETURNING) meets a row s1 changed. PostgreSQL's EvalPlanQual re-runs the row's own
# leaf's scan over the newest version (the other leaves blocked) and the SET over it: a row that still qualifies is
# updated from its newest version, one s1 moved out of the CTE's qual or deleted is skipped; when the recheck's SET
# moves the row to the other leaf it is deleted there and inserted through the root; a row s1 itself moved to another
# partition raises PostgreSQL's serialization error.

setup
{
 CREATE TABLE ruu (k int, v int, note text) PARTITION BY LIST (k);
 CREATE TABLE ruu1 PARTITION OF ruu FOR VALUES IN (1);
 CREATE TABLE ruu2 PARTITION OF ruu FOR VALUES IN (2);
 INSERT INTO ruu SELECT 1 + i % 2, i, 'r' || i FROM generate_series(1, 20) i;
 ANALYZE ruu;
}

teardown
{
 DROP TABLE ruu;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1u	{ UPDATE ruu SET note = note || '*' WHERE v IN (2, 3); }
step s1f	{ UPDATE ruu SET v = v + 50 WHERE v = 4; }
step s1d	{ DELETE FROM ruu WHERE v = 5; }
step s1x	{ UPDATE ruu SET k = 3 - k WHERE v = 6; }
step s1c	{ COMMIT; }

session s2
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s2u	{ WITH u AS (UPDATE ruu SET note = 'cte' WHERE v < 10) SELECT 1 AS done; }
step s2m	{ WITH u AS (UPDATE ruu SET k = 2, note = 'moved' WHERE v < 10) SELECT 1 AS done; }
step s2c	{ COMMIT; }

session s3
step s3s	{ SELECT tableoid::regclass AS leaf, k, v, note FROM ruu ORDER BY v, k; }
step s3e	{ EXPLAIN (COSTS OFF) WITH u AS (UPDATE ruu SET note = 'cte' WHERE v < 10) SELECT 1 AS done; }

permutation s3e s1u s1f s1d s2u s1c s2c s3s
permutation s1u s1f s1d s2m s1c s2c s3s
permutation s1x s2u s1c s2c s3s

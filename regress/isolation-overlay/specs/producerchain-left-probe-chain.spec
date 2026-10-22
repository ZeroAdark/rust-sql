# producerchain G6: an UPDATE / DELETE whose target is the inner of a nested loop whose outer is a LEFT hash join of two
# other relations (UPDATE t ... FROM a LEFT JOIN c ON .. : Nested Loop (Hash Left Join (a, c), Index Scan t)) meets a
# target row s1 changed. EvalPlanQual substitutes the target's newest version, refetches a's and c's rows at their junk
# ctids (c's none when the row came null-extended, the versions the statement joined: s1's change to c unseen) and reruns
# the join: a version that still joins is written over its new values, a deleted one is skipped, one whose key moved off
# the join is skipped, and a null-extended row stays null-extended. The ANTI form (the IS NULL test) deletes a row whose
# c row s1 inserted meanwhile: the recheck's c has no row, as EvalPlanQualFetchRowMark's NULL ctid gives none.
# producerchain G7: with no filter over the LEFT JOIN, PG joins c last (Nested Loop Left Join (Nested Loop (a, t),
# Materialize(c))): the recheck refills that spool from c's refetched row, or none for a null-extended row.

setup
{
 CREATE TABLE g6_t (id int PRIMARY KEY, v int);
 INSERT INTO g6_t SELECT i, i FROM generate_series(1, 100000) i;
 CREATE TABLE g6_a (id int, k int);
 INSERT INTO g6_a SELECT i * 9973, i % 7 FROM generate_series(1, 9) i;
 CREATE TABLE g6_c (k int, z int);
 INSERT INTO g6_c SELECT i, i FROM generate_series(1, 3) i;
}
setup { ANALYZE g6_t, g6_a, g6_c; }

teardown
{
 DROP TABLE g6_t, g6_a, g6_c;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1u	{ UPDATE g6_t SET v = v + 1 WHERE id = 9973; }
step s1n	{ UPDATE g6_t SET v = v + 1 WHERE id = 5 * 9973; }
step s1d	{ DELETE FROM g6_t WHERE id = 2 * 9973; }
step s1k	{ UPDATE g6_t SET id = id + 200000 WHERE id = 4 * 9973; }
step s1c	{ UPDATE g6_c SET z = 9 WHERE k = 1; }
step s1v	{ UPDATE g6_t SET v = v + 1 WHERE id = 4 * 9973; }
step s1i	{ INSERT INTO g6_c VALUES (4, 4); }
step s1x	{ COMMIT; }

session s2
setup		{ SET max_parallel_workers_per_gather = 0; BEGIN ISOLATION LEVEL READ COMMITTED; }
step s2u	{ UPDATE g6_t t SET v = -t.v - coalesce(c.z, 100) FROM g6_a a LEFT JOIN g6_c c ON a.k = c.k WHERE t.id = a.id AND coalesce(c.z, 0) < 3; }
step s2d	{ DELETE FROM g6_t t USING g6_a a LEFT JOIN g6_c c ON a.k = c.k WHERE t.id = a.id AND coalesce(c.z, 0) < 3; }
step s2a	{ DELETE FROM g6_t t USING g6_a a LEFT JOIN g6_c c ON a.k = c.k WHERE t.id = a.id AND c.k IS NULL; }
step s2l	{ UPDATE g6_t t SET v = -100 - coalesce(c.z, 50) FROM g6_a a LEFT JOIN g6_c c ON a.k = c.k WHERE t.id = a.id; }
step s2c	{ COMMIT; }

session s3
setup		{ SET max_parallel_workers_per_gather = 0; }
step s3e	{ EXPLAIN (COSTS OFF) UPDATE g6_t t SET v = -t.v - coalesce(c.z, 100) FROM g6_a a LEFT JOIN g6_c c ON a.k = c.k WHERE t.id = a.id AND coalesce(c.z, 0) < 3; }
step s3a	{ EXPLAIN (COSTS OFF) DELETE FROM g6_t t USING g6_a a LEFT JOIN g6_c c ON a.k = c.k WHERE t.id = a.id AND c.k IS NULL; }
step s3l	{ EXPLAIN (COSTS OFF) UPDATE g6_t t SET v = -100 - coalesce(c.z, 50) FROM g6_a a LEFT JOIN g6_c c ON a.k = c.k WHERE t.id = a.id; }
step s3r	{ SELECT id, v FROM g6_t WHERE v < 0 OR id % 9973 = 0 OR id > 100000 ORDER BY id; }

permutation s3e s1u s1d s1k s2u s1x s2c s3r
permutation s1u s1c s2u s1x s2c s3r
permutation s1n s2u s1x s2c s3r
permutation s1u s1k s2d s1x s2c s3r
permutation s2u s1u s2c s1x s3r
permutation s3a s1v s1i s2a s1x s2c s3r
permutation s3l s1u s1c s1d s2l s1x s2c s3r
permutation s1k s1i s2l s1x s2c s3r

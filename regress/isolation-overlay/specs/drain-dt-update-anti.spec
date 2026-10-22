# drain: an UPDATE or DELETE whose rows are the drained build rows of the target's own scan (`WHERE NOT EXISTS` with the
# filtered target hashed, the subquery's relation probing: a Hash Right Anti Join) under a concurrent change of a drained
# target row. The victim (s2) blocks on the row s1 changed; after s1 commits, PostgreSQL's EvalPlanQual reruns the plan
# with the newest version and the probe relation's NULL REFERENCE row (no row: the anti join hands the target row on):
#  (a) s1 updated the row's value: s2's UPDATE / DELETE acts on the newest version
#  (b) s1 moved the row out of the target's filter: s2 skips it
#  (c) s1 deleted the row: s2 skips it
#  (d) s1 updated the row and inserted the probe relation's matching row: s2 still acts (EvalPlanQual reads no probe row)
# Expected output is PostgreSQL 18.6's (the C oracle).

setup
{
 CREATE TABLE ddi_t (k int PRIMARY KEY, n int, v int);
 CREATE TABLE ddi_s (k int, w int);
 INSERT INTO ddi_t SELECT g, g, g * 10 FROM generate_series(1, 3000) g;
 INSERT INTO ddi_s SELECT g * 3, g FROM generate_series(1, 2000) g;
 ANALYZE ddi_t, ddi_s;
}

teardown
{
 DROP TABLE ddi_t, ddi_s;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1v	{ UPDATE ddi_t SET v = v + 1000 WHERE k = 2999; }
step s1n	{ UPDATE ddi_t SET n = 100 WHERE k = 2999; }
step s1del	{ DELETE FROM ddi_t WHERE k = 2999; }
step s1vs	{ UPDATE ddi_t SET v = v + 1000 WHERE k = 2999; INSERT INTO ddi_s VALUES (2999, 0); }
step s1c	{ COMMIT; }

session s2
setup
{
 SET enable_mergejoin = off;
 SET enable_nestloop = off;
}
step s2plan	{ EXPLAIN (costs off) UPDATE ddi_t t SET v = v + 1 WHERE NOT EXISTS (SELECT 1 FROM ddi_s s WHERE s.k = t.k) AND t.n > 2900; }
step s2upd	{ UPDATE ddi_t t SET v = v + 1 WHERE NOT EXISTS (SELECT 1 FROM ddi_s s WHERE s.k = t.k) AND t.n > 2900; }
step s2del	{ DELETE FROM ddi_t t WHERE NOT EXISTS (SELECT 1 FROM ddi_s s WHERE s.k = t.k) AND t.n > 2900; }
step s2chk	{ SELECT count(*), sum(v), min(k), max(k) FROM ddi_t WHERE n > 2900 OR k = 2999; }
step s2row	{ SELECT * FROM ddi_t WHERE k = 2999; }

permutation s2plan
permutation s1v s2upd s1c s2chk s2row
permutation s1n s2upd s1c s2chk s2row
permutation s1del s2upd s1c s2chk s2row
permutation s1vs s2upd s1c s2chk s2row
permutation s1v s2del s1c s2chk s2row
permutation s1n s2del s1c s2chk s2row

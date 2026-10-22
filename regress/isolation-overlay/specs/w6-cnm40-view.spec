# CNM-40 (modifytable): a DELETE whose EXISTS reads a view joining two relations plans as a Hash Semi Join over that
# join (rules#336's shape), so the write carries both inner relations' REFERENCE marks from the first match. It meets
# target rows s1 changed: a new v (the recheck refetches the inner rows by their carried ctids and still joins: deleted
# with its newest version), a new a the inner rows no longer match (stays), and an inner row s1 changed (the recheck
# reads the version the statement saw, as EvalPlanQualFetchRowMark does).

setup
{
 CREATE TABLE vt (id int PRIMARY KEY, a int, v int);
 INSERT INTO vt SELECT g, g % 10, g FROM generate_series(1, 40) g;
 CREATE TABLE vs (a int, b int);
 INSERT INTO vs SELECT g % 7, g % 5 FROM generate_series(1, 30) g;
 CREATE TABLE vu (b int, c int);
 INSERT INTO vu SELECT g, g * 10 FROM generate_series(0, 4) g;
 CREATE VIEW vv AS SELECT vs.a, vu.c FROM vs JOIN vu ON vs.b = vu.b;
 ANALYZE vt, vs, vu;
}
teardown
{
 DROP VIEW vv; DROP TABLE vt; DROP TABLE vs; DROP TABLE vu;
}
session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1v	{ UPDATE vt SET v = v + 1000 WHERE id IN (1, 2, 13); }
step s1key	{ UPDATE vt SET a = 9 WHERE id IN (3, 4); }
step s1inner	{ UPDATE vu SET c = 999 WHERE b = 2; }
step s1c	{ COMMIT; }
session s2
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; SET enable_mergejoin = off; SET enable_nestloop = off; }
step s2plan	{ EXPLAIN (costs off) DELETE FROM vt WHERE EXISTS (SELECT * FROM vv WHERE vv.a = vt.a AND vv.c = 20); }
step s2d	{ DELETE FROM vt WHERE EXISTS (SELECT * FROM vv WHERE vv.a = vt.a AND vv.c = 20) RETURNING id, a, v; }
step s2c	{ COMMIT; }
session s3
step s3sel	{ SELECT count(*), sum(v), sum(a) FROM vt; }
permutation s2plan s1c s2c
permutation s1v s2d s1c s2c s3sel
permutation s1key s2d s1c s2c s3sel
permutation s1inner s1v s2d s1c s2c s3sel

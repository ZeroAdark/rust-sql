# D3665: UPDATE through a WITH CHECK OPTION view whose qual is EXISTS over another relation, the view over an
# inheritance parent with a child (two result relations), planned as a Hash Right Semi Join with the Append of the two
# targets' scans hashed and the other relation probing (nested loops and merge joins off), while another session changes
# the target row (READ COMMITTED). PostgreSQL 18.6's EvalPlanQual rescans the row's own relation at its newest version
# (the other empty) and the probing relation at the row its REFERENCE mark fetched, then the check option's SubPlan runs
# over the new row.
#  (a) the row updated in place, still joined: written, the check passes
#  (b) the row updated in place, its join key moved off the fetched row: skipped
#  (c) the row updated in place, the new row failing the check option: the check's error
# The EXPLAIN step pins the plan. Expected output is PostgreSQL 18.6's (the C oracle).
setup
{
 CREATE TABLE cv_kr (a int, t text);
 INSERT INTO cv_kr VALUES (1, 'one'), (2, 'two'), (3, 'three'), (4, 'four');
 INSERT INTO cv_kr SELECT i, 'f' FROM generate_series(100, 160) i;
 CREATE TABLE cv_ip (id int, a int, n text);
 CREATE TABLE cv_ic () INHERITS (cv_ip);
 INSERT INTO cv_ip VALUES (1, 1, 'p1'), (11, 3, 'p11');
 INSERT INTO cv_ic VALUES (2, 2, 'c2'), (12, 2, 'c12');
 CREATE VIEW cv_ipv AS SELECT * FROM cv_ip p WHERE EXISTS (SELECT 1 FROM cv_kr r WHERE r.a = p.a) WITH CHECK OPTION;
 ANALYZE cv_kr, cv_ip, cv_ic;
}

teardown
{
 DROP VIEW cv_ipv;
 DROP TABLE cv_ip, cv_ic, cv_kr;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1n	{ UPDATE cv_ip SET n = 'c' WHERE id = 12; }
step s1a	{ UPDATE cv_ip SET a = 1 WHERE id = 12; }
step s1c	{ COMMIT; }

session s2
setup		{ SET enable_nestloop = off; SET enable_mergejoin = off; }
step s2exp	{ EXPLAIN (COSTS OFF) UPDATE cv_ipv SET a = a + 1 WHERE id > 10; }
step s2u	{ UPDATE cv_ipv SET a = a + 1 WHERE id > 10; }
step s2bad	{ UPDATE cv_ipv SET a = 9 WHERE id = 12; }
step s2sel	{ SELECT tableoid::regclass, id, a, n FROM cv_ip ORDER BY id; }

permutation s2exp s1n s2u s1c s2sel
permutation s1a s2u s1c s2sel
permutation s1n s2bad s1c s2sel

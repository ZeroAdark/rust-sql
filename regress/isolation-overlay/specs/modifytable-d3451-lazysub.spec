# modifytable D3451 LAZYSUB: a rechecking UPDATE whose scan filter reads a lazy SubPlan, under OR after a plain clause:
# a hashed one (s2h / s2z: EXISTS over s.a - 1, PostgreSQL's hashed SubPlan probed with ANY) or a per-row one (s2p: an
# inequality, no hash). s1 holds row 2 (a 11); s2's UPDATE waits on it and, after s1 commits, EvalPlanQual re-evaluates
# the filter over the newest version: the OR's plain clause first, the SubPlan only where the OR reaches it, its scan
# under s2's statement snapshot (s1's new versions unseen, row 2's old version seen: s1a12 still finds a = 11). s2z's
# plain clause holds for every row on the first pass, so the hashed SubPlan is first evaluated in the recheck. The rows
# are read back by s3.

setup
{
 CREATE TABLE ls_t (id int PRIMARY KEY, a int NOT NULL, b int NOT NULL);
 INSERT INTO ls_t VALUES (1, 10, 0), (2, 11, 0), (3, 20, 0), (4, 31, 0), (5, 40, 0), (6, 50, 0);
}

teardown
{
 DROP TABLE ls_t;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1a21	{ UPDATE ls_t SET a = 21 WHERE id = 2; }
step s1a12	{ UPDATE ls_t SET a = 12 WHERE id = 2; }
step s1a33	{ UPDATE ls_t SET a = 33 WHERE id = 2; }
step s1b100	{ UPDATE ls_t SET b = 100 WHERE id = 2; }
step s1b7	{ UPDATE ls_t SET b = 7 WHERE id = 2; }
step s1b7a35	{ UPDATE ls_t SET b = 7, a = 35 WHERE id = 2; }
step s1c	{ COMMIT; }

session s2
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s2e	{ EXPLAIN (COSTS OFF) UPDATE ls_t s SET b = b + 1 WHERE s.id <= 4 AND (s.b = 100 OR EXISTS (SELECT 1 FROM ls_t s2 WHERE s2.a = s.a - 1)); }
step s2ep	{ EXPLAIN (COSTS OFF) UPDATE ls_t s SET b = b + 1 WHERE s.id <= 4 AND (s.b = 100 OR EXISTS (SELECT 1 FROM ls_t s2 WHERE s2.a > s.a + 25)); }
step s2h	{ UPDATE ls_t s SET b = b + 1 WHERE s.id <= 4 AND (s.b = 100 OR EXISTS (SELECT 1 FROM ls_t s2 WHERE s2.a = s.a - 1)); }
step s2z	{ UPDATE ls_t s SET b = b + 1 WHERE s.id <= 4 AND (s.b = 0 OR EXISTS (SELECT 1 FROM ls_t s2 WHERE s2.a = s.a - 1)); }
step s2p	{ UPDATE ls_t s SET b = b + 1 WHERE s.id <= 4 AND (s.b = 100 OR EXISTS (SELECT 1 FROM ls_t s2 WHERE s2.a > s.a + 25)); }
step s2c	{ COMMIT; }

session s3
step s3sel	{ SELECT id, a, b FROM ls_t ORDER BY id; }

permutation s2e s2ep s1c s2c
permutation s1a21 s2h s1c s2c s3sel
permutation s1a12 s2h s1c s2c s3sel
permutation s1a33 s2h s1c s2c s3sel
permutation s1b100 s2h s1c s2c s3sel
permutation s1b7 s2z s1c s2c s3sel
permutation s1b7a35 s2z s1c s2c s3sel
permutation s1a12 s2p s1c s2c s3sel
permutation s1a33 s2p s1c s2c s3sel

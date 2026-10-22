# D3705: UPDATE / DELETE planned as a Nested Loop whose outer is a nest of COPY-marked rows (two CTE Scans of one
# MATERIALIZED CTE, or two Function Scans) and whose inner is the target's scan (hash and merge joins and Materialize off),
# while another session changes the target row (READ COMMITTED). PostgreSQL 18.6's EvalPlanQual hands each CTE /
# Function Scan its stored row and the target's scan the newest version, the join quals checked again over them.
#  (a) the row updated in place, its key unchanged: written over the newest version
#  (b) the row's key moved off the stored rows' value: skipped
#  (c) a DELETE over the row updated in place: deleted
#  (d) the Function Scan form over the row updated in place: written
#  (e) the Function Scan form joined on the inner function's column, the row's value moved off the stored row's (another
#      of the function's rows would match): skipped
# The EXPLAIN steps pin the plans. Expected output is PostgreSQL 18.6's (the C oracle).
setup
{
 CREATE TABLE cr_k (id int, a int);
 INSERT INTO cr_k SELECT i, i % 7 FROM generate_series(1, 40) i;
 ANALYZE cr_k;
 CREATE TABLE cr_t (n int, v int);
 INSERT INTO cr_t VALUES (1, 1), (2, 2), (5, 5);
}

teardown
{
 DROP TABLE cr_t, cr_k;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1v	{ UPDATE cr_t SET v = v + 10 WHERE n = 2; }
step s1n	{ UPDATE cr_t SET n = 3 WHERE n = 2; }
step s1w	{ UPDATE cr_t SET v = 3 WHERE n = 2; }
step s1c	{ COMMIT; }

session s2
setup		{ SET enable_hashjoin = off; SET enable_mergejoin = off; SET enable_material = off; }
step s2exp	{ EXPLAIN (COSTS OFF) WITH c AS MATERIALIZED (SELECT id, a FROM cr_k) UPDATE cr_t SET v = v + 100 FROM c c1, c c2 WHERE cr_t.n = c1.id AND c2.id = c1.id AND c1.id < 3; }
step s2u	{ WITH c AS MATERIALIZED (SELECT id, a FROM cr_k) UPDATE cr_t SET v = v + 100 FROM c c1, c c2 WHERE cr_t.n = c1.id AND c2.id = c1.id AND c1.id < 3 RETURNING cr_t.n, cr_t.v, c1.a; }
step s2d	{ WITH c AS MATERIALIZED (SELECT id, a FROM cr_k) DELETE FROM cr_t USING c c1, c c2 WHERE cr_t.n = c1.id AND c2.id = c1.id AND c1.id = 2 RETURNING cr_t.n, cr_t.v; }
step s2fexp	{ EXPLAIN (COSTS OFF) UPDATE cr_t SET v = v + s.b FROM (SELECT a, b FROM generate_series(1, 3) a, generate_series(1, 3) b) s WHERE cr_t.n = 2 AND s.a = 3 AND s.b > 1; }
step s2f	{ UPDATE cr_t SET v = v + s.b FROM (SELECT a, b FROM generate_series(1, 3) a, generate_series(1, 3) b) s WHERE cr_t.n = 2 AND s.a = 3 AND s.b > 1 RETURNING cr_t.n, cr_t.v, s.b; }
step s2gexp	{ EXPLAIN (COSTS OFF) UPDATE cr_t SET n = n + 10 FROM (SELECT a, b FROM generate_series(1, 3) a, generate_series(1, 3) b) s WHERE cr_t.v = s.b AND s.a = 3; }
step s2g	{ UPDATE cr_t SET n = n + 10 FROM (SELECT a, b FROM generate_series(1, 3) a, generate_series(1, 3) b) s WHERE cr_t.v = s.b AND s.a = 3 RETURNING cr_t.n, cr_t.v, s.b; }
step s2sel	{ SELECT n, v FROM cr_t ORDER BY n; }

permutation s2exp s1v s2u s1c s2sel
permutation s1n s2u s1c s2sel
permutation s1v s2d s1c s2sel
permutation s2fexp s1v s2f s1c s2sel
permutation s2gexp s1w s2g s1c s2sel

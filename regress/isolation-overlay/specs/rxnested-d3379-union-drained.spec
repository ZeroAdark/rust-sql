# D3379 UNION-DRAINED: an UPDATE / DELETE of an inheritance parent FROM a UNION ALL subquery whose second arm the planner
# keeps as a Subquery Scan over an Append of its own (a WHERE over an inheritance parent, its value a + 1 that may
# raise), joined by an OR qual: PostgreSQL 18.6 plans a Nested Loop whose outer is the union's Append and whose inner is
# a Materialize over the Append of the result relations' scans. Another session changes target rows first (READ
# COMMITTED). The subquery's COPY mark names no relation a scan carries, so EvalPlanQual scans every arm again, whole,
# the nested one too, and joins them with the target's newest version. it_c 2 matches only the nested arm's row (1 + 1):
# it is updated after its concurrent change only when the recheck runs that arm; it 1 loses its match (b now a) and is
# skipped; it 3 moves to (2, b), matched again only by the nested arm. Expected output is PostgreSQL 18.6's.

setup
{
 CREATE TABLE ud_src (a int);
 INSERT INTO ud_src VALUES (0);
 CREATE TABLE ud_src_child () INHERITS (ud_src);
 INSERT INTO ud_src_child VALUES (1);
 CREATE TABLE ud_t (a int, b char, c text);
 CREATE TABLE ud_t_c (d int) INHERITS (ud_t);
 INSERT INTO ud_t VALUES (1, 'b', 'p'), (2, 'a', 'p'), (3, 'a', 'p');
 INSERT INTO ud_t_c VALUES (1, 'a', 'p', 9), (2, 'b', 'p', 9), (3, 'b', 'p', 9);
}

teardown
{
 DROP TABLE ud_t, ud_src CASCADE;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1keep	{ UPDATE ud_t_c SET c = 'x' WHERE a = 2; }
step s1miss	{ UPDATE ONLY ud_t SET b = 'a' WHERE a = 1; }
step s1move	{ UPDATE ONLY ud_t SET a = 2, b = 'b' WHERE a = 3; }
step s1del	{ DELETE FROM ud_t_c WHERE a = 3; }
step s1c	{ COMMIT; }

session s2
setup		{ SET max_parallel_workers_per_gather = 0; BEGIN ISOLATION LEVEL READ COMMITTED; }
step s2exp	{ EXPLAIN (COSTS OFF) UPDATE ud_t SET c = 'f' FROM (SELECT a FROM ud_src UNION ALL SELECT a + 1 FROM ud_src WHERE a > 0) ss (a) WHERE (ud_t.a = ss.a AND ud_t.b = 'b') OR ud_t.a = 3; }
step s2upd	{ UPDATE ud_t SET c = 'f' FROM (SELECT a FROM ud_src UNION ALL SELECT a + 1 FROM ud_src WHERE a > 0) ss (a) WHERE (ud_t.a = ss.a AND ud_t.b = 'b') OR ud_t.a = 3; }
step s2del	{ DELETE FROM ud_t USING (SELECT a FROM ud_src UNION ALL SELECT a + 1 FROM ud_src WHERE a > 0) ss (a) WHERE (ud_t.a = ss.a AND ud_t.b = 'b') OR ud_t.a = 3; }
step s2c	{ COMMIT; }

session s3
step s3sel	{ SELECT tableoid::regclass::text AS rel, a, b, c FROM ud_t ORDER BY 1, 2, 3; }

permutation s2exp s1keep s1miss s1move s2upd s1c s2c s3sel
permutation s1keep s1miss s1del s2del s1c s2c s3sel

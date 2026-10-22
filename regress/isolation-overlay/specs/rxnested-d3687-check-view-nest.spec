# D3687: UPDATE through a WITH CHECK OPTION view whose qual is EXISTS over another relation, planned as a Nested Loop
# Semi Join with the target's scan the outer and a Materialize over the other relation's scan the inner (hash and merge
# joins off), the check option's SubPlan step before the write, while another session changes the target row (READ
# COMMITTED). PostgreSQL 18.6's EvalPlanQual reruns the join over the newest version and the inner relation's row its
# REFERENCE mark fetched, then the check option's SubPlan runs over the new row.
#  (a) the row updated in place, still joined: written, the check passes
#  (b) the row's key moved off the statement's qual: skipped
#  (c) the row updated in place, the new row failing the check option: the check's error
# The EXPLAIN step pins the plan. Expected output is PostgreSQL 18.6's (the C oracle).
setup
{
 CREATE TABLE cn_kr (a int, t text);
 INSERT INTO cn_kr VALUES (1, 'one'), (2, 'two'), (3, 'three'), (4, 'four');
 CREATE TABLE cn_t (id int, a int, n text);
 INSERT INTO cn_t VALUES (1, 1, 'r1'), (11, 2, 'r11'), (12, 2, 'r12');
 CREATE VIEW cn_v AS SELECT * FROM cn_t t WHERE EXISTS (SELECT 1 FROM cn_kr r WHERE r.a = t.a) WITH CHECK OPTION;
}

teardown
{
 DROP VIEW cn_v;
 DROP TABLE cn_t, cn_kr;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1n	{ UPDATE cn_t SET n = 'c' WHERE id = 12; }
step s1a	{ UPDATE cn_t SET a = 3 WHERE id = 12; }
step s1c	{ COMMIT; }

session s2
setup		{ SET enable_hashjoin = off; SET enable_mergejoin = off; }
step s2exp	{ EXPLAIN (COSTS OFF) UPDATE cn_v SET n = n || '+' WHERE a = 2; }
step s2u	{ UPDATE cn_v SET n = n || '+' WHERE a = 2; }
step s2bad	{ UPDATE cn_v SET a = 9 WHERE a = 2 AND id = 12; }
step s2sel	{ SELECT id, a, n FROM cn_t ORDER BY id; }

permutation s2exp s1n s2u s1c s2sel
permutation s1a s2u s1c s2sel
permutation s1n s2bad s1c s2sel

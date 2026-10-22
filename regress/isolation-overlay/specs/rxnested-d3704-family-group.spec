# D3704: DELETE / UPDATE whose EXISTS over an inheritance parent PostgreSQL plans as a Nested Loop: outer a HashAggregate
# (the semi join made unique) over the Append of the parent's and its children's index scans, inner the target's scan
# (hash and merge joins, Materialize, Memoize, sequential and bitmap scans off), while another session changes the target
# row (READ COMMITTED). PostgreSQL 18.6's EvalPlanQual refetches the child row the row's tableoid names (the other
# children give none), the HashAggregate over it yields the one group, joined to the newest version.
#  (a) the row updated in place, its key unchanged: deleted
#  (b) the row's key moved to another group's value: skipped
#  (c) an UPDATE over the row updated in place: written over the newest version
# The EXPLAIN step pins the plan. Expected output is PostgreSQL 18.6's (the C oracle).
setup
{
 CREATE TABLE fg_p (id int, v text);
 CREATE TABLE fg_p1 () INHERITS (fg_p);
 CREATE TABLE fg_p2 () INHERITS (fg_p);
 INSERT INTO fg_p SELECT g, 'p' || g FROM generate_series(1, 20) g;
 INSERT INTO fg_p1 SELECT g % 8, 'q' || g FROM generate_series(1, 40) g;
 INSERT INTO fg_p2 SELECT g % 5, 'r' || g FROM generate_series(1, 30) g;
 CREATE INDEX fg_p_id ON fg_p (id);
 CREATE INDEX fg_p1_id ON fg_p1 (id);
 CREATE INDEX fg_p2_id ON fg_p2 (id);
 CREATE TABLE fg_t (k int, t text);
 INSERT INTO fg_t VALUES (1, 'a'), (2, 'b'), (3, 'c'), (9, 'd');
}
setup { VACUUM ANALYZE fg_p; }
setup { VACUUM ANALYZE fg_p1; }
setup { VACUUM ANALYZE fg_p2; }

teardown
{
 DROP TABLE fg_t, fg_p CASCADE;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1t	{ UPDATE fg_t SET t = t || '!' WHERE k = 2; }
step s1k	{ UPDATE fg_t SET k = 3 WHERE k = 2; }
step s1c	{ COMMIT; }

session s2
setup		{ SET enable_hashjoin = off; SET enable_mergejoin = off; SET enable_material = off; SET enable_memoize = off; SET enable_seqscan = off; SET enable_bitmapscan = off; }
step s2exp	{ EXPLAIN (COSTS OFF) DELETE FROM fg_t o WHERE EXISTS (SELECT 1 FROM fg_p p WHERE p.id = o.k AND p.v LIKE 'q%'); }
step s2d	{ DELETE FROM fg_t o WHERE EXISTS (SELECT 1 FROM fg_p p WHERE p.id = o.k AND p.v LIKE 'q%'); }
step s2u	{ UPDATE fg_t o SET t = t || '+' WHERE EXISTS (SELECT 1 FROM fg_p p WHERE p.id = o.k AND p.v LIKE 'q%'); }
step s2sel	{ SELECT k, t FROM fg_t ORDER BY k, t; }

permutation s2exp s1t s2d s1c s2sel
permutation s1k s2d s1c s2sel
permutation s1t s2u s1c s2sel

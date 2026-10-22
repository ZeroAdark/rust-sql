# D1337(A) (M8-RERUN row A): a correlated SubPlan whose inner hash join rebuilds its hash table at every call (its key
# reads the locked row's column, or the build is a set function over it), below LockRows and an UPDATE, under the
# EvalPlanQual recheck. s1 updates the locked row's key column and both of the SubPlan's tables, then commits; the
# recheck evaluates the SubPlan again with the row's newest version as its parameter, its own scans under the
# statement's snapshot (PostgreSQL's PlanRowMarks name the top-level relation alone, D1478), so the rebuilt table holds
# the old table_b / table_c rows keyed by the new n. The joins are hashed (s2 turns nested loops, merge joins, Memoize
# and Material off). Expected output is the C oracle's (PostgreSQL 18.6).

setup
{
 CREATE TABLE table_a (id integer, n integer, value text);
 CREATE TABLE table_b (k integer, g integer, v integer);
 CREATE TABLE table_c (g integer, y integer);
 INSERT INTO table_a VALUES (1, 1, 'tableAValue'), (2, 2, 'other');
 INSERT INTO table_b SELECT i % 3, i % 5, i FROM generate_series(1, 30) i;
 INSERT INTO table_c SELECT i % 6, i FROM generate_series(1, 6) i;
 ANALYZE table_a, table_b, table_c;
}

teardown
{
 DROP TABLE table_a, table_b, table_c;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step upd	{
	UPDATE table_a SET n = n + 1, value = 'newTableAValue' WHERE id = 1;
	UPDATE table_b SET v = v + 100 WHERE k = 1;
	UPDATE table_c SET y = y + 10;
}
step c1		{ COMMIT; }

session s2
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; SET LOCAL enable_nestloop = off; SET LOCAL enable_mergejoin = off; SET LOCAL enable_memoize = off; SET LOCAL enable_material = off; }
step keyv	{ SELECT ta.id, ta.n, ta.value, (SELECT sum(tb.v + tc.y) FROM table_b tb JOIN table_c tc ON tb.g = tc.g + ta.n WHERE tb.k = ta.id) AS s FROM table_a ta WHERE ta.id = 1 FOR UPDATE; }
step srfv	{ SELECT ta.id, ta.n, (SELECT sum(tb.v) FROM generate_series(0, ta.n) s JOIN table_b tb ON tb.g = s WHERE tb.k = ta.id) AS s FROM table_a ta WHERE ta.id = 1 FOR UPDATE; }
step updv	{ UPDATE table_a ta SET value = ta.value || '/' || (SELECT count(*) FROM table_b tb JOIN table_c tc ON tb.g = tc.g + ta.n WHERE tb.k = ta.id) WHERE ta.id = 1 RETURNING ta.id, ta.n, ta.value; }
step c2		{ COMMIT; }

permutation upd keyv c1 c2
permutation upd srfv c1 c2
permutation upd updv c1 c2

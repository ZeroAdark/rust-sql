# D3706: UPDATE / DELETE planned as a Nested Loop whose outer is a nest of another relation's scan and the target's scan
# and whose inner is the Function Scan of a VOLATILE set-returning function (hash and merge joins and Materialize off),
# while another session changes the target row (READ COMMITTED). PostgreSQL 18.6's EvalPlanQual refetches the other
# relation's row, hands the target's scan the newest version and the Function Scan its stored row (the function is not
# called again), the join quals checked again over them.
#  (a) the row updated in place, its value unchanged: written over the newest version
#  (b) the row's value moved off the stored function row: skipped
#  (c) a DELETE over the row updated in place: deleted
# The EXPLAIN step pins the plan; the sequence the function advances shows one call. Expected output is PostgreSQL
# 18.6's (the C oracle).
setup
{
 CREATE TABLE fa_o (x int PRIMARY KEY);
 INSERT INTO fa_o VALUES (1), (2), (3), (4), (5);
 ANALYZE fa_o;
 CREATE SEQUENCE fa_q;
 CREATE FUNCTION fa_pl() RETURNS SETOF int LANGUAGE plpgsql VOLATILE AS $$ BEGIN PERFORM nextval('fa_q'); RETURN QUERY SELECT i FROM generate_series(1, 3) i; END $$;
 CREATE TABLE fa_t (x int, g int);
 INSERT INTO fa_t VALUES (1, 1), (2, 1), (2, 3), (4, 2);
}

teardown
{
 DROP TABLE fa_t, fa_o;
 DROP FUNCTION fa_pl();
 DROP SEQUENCE fa_q;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1s	{ UPDATE fa_t SET g = g WHERE x = 2 AND g = 1; }
step s1g	{ UPDATE fa_t SET g = 2 WHERE x = 2 AND g = 1; }
step s1c	{ COMMIT; }

session s2
setup		{ SET enable_hashjoin = off; SET enable_mergejoin = off; SET enable_material = off; }
step s2exp	{ EXPLAIN (COSTS OFF) UPDATE fa_t t SET g = t.g + 10 FROM fa_o o, fa_pl() f WHERE t.x = o.x AND t.g = f; }
step s2u	{ UPDATE fa_t t SET g = t.g + 10 FROM fa_o o, fa_pl() f WHERE t.x = o.x AND t.g = f RETURNING t.x, t.g, f; }
step s2d	{ DELETE FROM fa_t t USING fa_o o, fa_pl() f WHERE t.x = o.x AND t.g = f AND o.x > 1 RETURNING t.x, t.g, f; }
step s2sel	{ SELECT x, g FROM fa_t ORDER BY x, g; }
step s2seq	{ SELECT last_value FROM fa_q; }

permutation s2exp s1s s2u s1c s2sel s2seq
permutation s1g s2u s1c s2sel s2seq
permutation s1s s2d s1c s2sel s2seq

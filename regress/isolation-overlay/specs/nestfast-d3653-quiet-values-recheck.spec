# D3653 (nestfast): SELECT .. FOR UPDATE OF t over a Nested Loop whose inner is a Materialize over a constant VALUES
# list. Under READ COMMITTED a concurrent update of a locked row makes the lock recheck it (EvalPlanQual): the recheck
# joins the newest version to the VALUES row it joined (the list's COPY mark), never to the whole list again. A changed
# join column drops one of a row's two output rows (1 joined (3, p) and (5, q); 4 joins only (5, q)); a row moved out
# of the filter drops both. A lock over the nest itself (no Sort) is still refused (D3656).

setup
{
  CREATE TABLE d3653_t (a int4, b text);
  INSERT INTO d3653_t SELECT g, 'r' || g FROM generate_series(1, 40) g;
  ANALYZE d3653_t;
}

teardown
{
  DROP TABLE d3653_t;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1k	{ UPDATE d3653_t SET a = 4 WHERE a = 1; }
step s1f	{ UPDATE d3653_t SET a = 9 WHERE a = 2; }
step s1b	{ UPDATE d3653_t SET b = 'n3' WHERE a = 3; }
step s1c	{ COMMIT; }

session s2
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; SET LOCAL enable_hashjoin = off; SET LOCAL enable_mergejoin = off; SET LOCAL enable_memoize = off; }
step s2x	{ EXPLAIN (COSTS OFF) SELECT t.a, v.x FROM d3653_t t JOIN (VALUES (3, 'p'), (5, 'q')) v(n, x) ON t.a < v.n WHERE t.a < 6 ORDER BY 1, 2 FOR UPDATE OF t; }
step s2l	{ SELECT t.a, t.b, v.x FROM d3653_t t JOIN (VALUES (3, 'p'), (5, 'q')) v(n, x) ON t.a < v.n WHERE t.a < 6 ORDER BY 1, 3 FOR UPDATE OF t; }
step s2c	{ COMMIT; }

permutation s2x s2c s1c
permutation s1k s2l s1c s2c
permutation s1f s2l s1c s2c
permutation s1b s2l s1c s2c
permutation s1k s1f s2l s1c s2c

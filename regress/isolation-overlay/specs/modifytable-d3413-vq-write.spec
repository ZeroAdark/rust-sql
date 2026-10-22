# modifytable D3413 VQ-WRITE: an UPDATE / DELETE whose target's own scan is the outer of a nested loop over a Material of
# a constant VALUES list (the list COPY-marked; material V-QUIET's eager fill), the join a non-equi qual no hash or merge
# join takes. s1 changes rows s2's write waits on (READ COMMITTED): EvalPlanQual reruns the plan with the target's scan
# returning the newest version and the Values Scan its stored row (the row the Material held and the nest handed on), so
# a version moved out of the target's own quals is skipped, one that still joins is written over its newest values, and a
# deleted row gives none. A target row two list rows join is written once (the second meets its own update).
# Expected output is the C oracle's (PostgreSQL 18, pgdg).
setup
{
  CREATE TABLE vq (a int, b text);
  INSERT INTO vq SELECT g, 'r' || g FROM generate_series(1, 40) g;
  ANALYZE vq;
}

teardown
{
  DROP TABLE vq;
}

session s1
setup		{ BEGIN; }
step s1u	{ UPDATE vq SET a = a + 100 WHERE a = 34; UPDATE vq SET b = 's1' WHERE a = 35; }
step s1d	{ DELETE FROM vq WHERE a = 37; }
step s1c	{ COMMIT; }

session s2
setup		{ SET enable_hashjoin = off; SET enable_mergejoin = off; SET enable_memoize = off; }
step s2x	{ EXPLAIN (COSTS OFF) UPDATE vq t SET b = t.b || v.x FROM (VALUES (33, 'p'), (36, 'q')) v(n, x) WHERE t.a > v.n AND t.a < 38; }
step s2u	{ UPDATE vq t SET b = t.b || v.x FROM (VALUES (33, 'p'), (36, 'q')) v(n, x) WHERE t.a > v.n AND t.a < 38; }
step s2d	{ DELETE FROM vq t USING (VALUES (33, 'p'), (36, 'q')) v(n, x) WHERE t.a > v.n AND t.a < 38; }
step s2s	{ SELECT a, b FROM vq WHERE a > 30 ORDER BY a; }

# the plan: Nested Loop, the target's scan outer, Materialize over the Values Scan inner
permutation s2x s1c
# row 34 moved out of the target's quals (skipped), row 35 written over s1's b
permutation s1u s2u s1c s2s
# the same under a DELETE
permutation s1u s2d s1c s2s
# a deleted row gives none; row 37 joined twice is not written
permutation s1d s2u s1c s2s

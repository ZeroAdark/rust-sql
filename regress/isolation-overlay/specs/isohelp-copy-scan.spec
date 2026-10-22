# An UPDATE / DELETE whose rows join the target's own index scan (the nested loop's outer) to a Function Scan or a CTE
# Scan (its inner, no Materialize): the function / CTE relation carries a ROW_MARK_COPY mark. EvalPlanQual hands that
# scan its stored row (EvalPlanQualFetchRowMark's copy leg), rechecks the scan's quals and the join over the target's
# newest version, and never runs the function or the CTE again. Arms: the row updated (recheck passes), its key moved
# (recheck fails: skipped), deleted (skipped). Expected output is PostgreSQL 18.6's.

setup
{
 CREATE TABLE cst (k int PRIMARY KEY, v int);
 INSERT INTO cst VALUES (1, 10), (2, 20), (3, 30);
 CREATE TABLE csc (k int, d int);
 INSERT INTO csc VALUES (1, 5), (2, 6), (3, 7);
}

teardown
{
 DROP TABLE cst;
 DROP TABLE csc;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1u	{ UPDATE cst SET v = v + 100 WHERE k = 1; }
step s1k	{ UPDATE cst SET k = 9 WHERE k = 1; }
step s1d	{ DELETE FROM cst WHERE k = 1; }
step s1c	{ COMMIT; }

session s2
step fexpl	{ EXPLAIN (COSTS OFF) UPDATE cst SET v = cst.v + g.k FROM generate_series(1, 2) AS g(k) WHERE cst.k = g.k AND cst.k = 1; }
step fupd	{ UPDATE cst SET v = cst.v + g.k FROM generate_series(1, 2) AS g(k) WHERE cst.k = g.k AND cst.k = 1 RETURNING cst.k, cst.v, g.k; }
step fdel	{ DELETE FROM cst USING generate_series(1, 3) AS g(k) WHERE cst.k = g.k AND cst.k = 1 RETURNING cst.k, cst.v, g.k; }
step cexpl	{ EXPLAIN (COSTS OFF) WITH src AS MATERIALIZED (SELECT k, d FROM csc) UPDATE cst SET v = cst.v + src.d FROM src WHERE cst.k = src.k AND cst.k = 1; }
step cupd	{ WITH src AS MATERIALIZED (SELECT k, d FROM csc) UPDATE cst SET v = cst.v + src.d FROM src WHERE cst.k = src.k AND cst.k = 1 RETURNING cst.k, cst.v, src.d; }
step cdel	{ WITH src AS MATERIALIZED (SELECT k, d FROM csc) DELETE FROM cst USING src WHERE cst.k = src.k AND cst.k = 1 RETURNING cst.k, src.d; }

session s3
step sel	{ SELECT k, v FROM cst ORDER BY k; }

permutation fexpl s1u fupd s1c sel
permutation s1k fupd s1c sel
permutation s1d fupd s1c sel
permutation s1u fdel s1c sel
permutation s1k fdel s1c sel
permutation cexpl s1u cupd s1c sel
permutation s1k cupd s1c sel
permutation s1d cupd s1c sel
permutation s1u cdel s1c sel
permutation s1k cdel s1c sel

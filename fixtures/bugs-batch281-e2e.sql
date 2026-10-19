-- Differential fixture for the 2026-09-13 audit bug batch 281
-- (backend/optimizer/plan: catchable statistics-data load errors, and
-- debug_print_plan field parity — ANY-sublink join Var varnosyn, Aggref
-- dependency order, WindowFunc runCondition kept as-is, and inlined CTE
-- expression locations). Run via scripts/bugs-batch281-e2e.sh against the
-- frozen expected captured from C PostgreSQL 18.6.
\pset pager off

-- regression: a stable function evaluated while estimating the
-- first relation drops the MCV statistics object the second relation's
-- estimate then loads; statext_mcv_load (mcv.c:566) reports a catchable
-- 'cache lookup failed' XX000, not a backend abort under the "not yet
-- built" message
CREATE TABLE b281_s1 (a int, b int);
INSERT INTO b281_s1 SELECT i % 10, i % 10 FROM generate_series(1, 1000) i;
CREATE STATISTICS b281_s1_mcv (mcv) ON a, b FROM b281_s1;
ANALYZE b281_s1;
CREATE TABLE b281_s2 (x int);
INSERT INTO b281_s2 VALUES (1);
ANALYZE b281_s2;
CREATE FUNCTION b281_dropit() RETURNS int LANGUAGE plpgsql VOLATILE AS
$$ BEGIN DROP STATISTICS b281_s1_mcv; RETURN 1; END $$;
CREATE FUNCTION b281_g() RETURNS int LANGUAGE plpgsql STABLE AS
$$ BEGIN RETURN b281_dropit(); END $$;
DO $$
BEGIN
  PERFORM * FROM b281_s2, b281_s1 WHERE b281_s2.x = b281_g() AND b281_s1.a = 1 AND b281_s1.b = 1;
EXCEPTION WHEN OTHERS THEN
  RAISE NOTICE '% %', SQLSTATE, regexp_replace(SQLERRM, '\d+$', 'N');
END $$;
SELECT count(*) FROM pg_statistic_ext WHERE stxname = 'b281_s1_mcv';
-- same window, functional dependencies (dependencies.c:629)
DROP STATISTICS b281_s1_mcv;
CREATE STATISTICS b281_s1_mcv (dependencies) ON a, b FROM b281_s1;
ANALYZE b281_s1;
DO $$
BEGIN
  PERFORM * FROM b281_s2, b281_s1 WHERE b281_s2.x = b281_g() AND b281_s1.a = 1 AND b281_s1.b = 1;
EXCEPTION WHEN OTHERS THEN
  RAISE NOTICE '% %', SQLSTATE, regexp_replace(SQLERRM, '\d+$', 'N');
END $$;
-- same window, ndistinct (mvdistinct.c:158)
DROP STATISTICS b281_s1_mcv;
CREATE STATISTICS b281_s1_mcv (ndistinct) ON a, b FROM b281_s1;
ANALYZE b281_s1;
DO $$
BEGIN
  PERFORM count(*) FROM b281_s2, b281_s1 WHERE b281_s2.x = b281_g() GROUP BY b281_s1.a, b281_s1.b;
EXCEPTION WHEN OTHERS THEN
  RAISE NOTICE '% %', SQLSTATE, regexp_replace(SQLERRM, '\d+$', 'N');
END $$;
SELECT count(*) FROM pg_statistic_ext WHERE stxname = 'b281_s1_mcv';
DROP TABLE b281_s1, b281_s2;
DROP FUNCTION b281_g(), b281_dropit();

-- debug_print_plan parity (jit off: pgrust's jit default differs)
CREATE TABLE b281_t1 (a int, b int);
CREATE TABLE b281_t2 (x int);
INSERT INTO b281_t1 VALUES (1, 1), (2, 2);
INSERT INTO b281_t2 VALUES (1);
ANALYZE b281_t1;
ANALYZE b281_t2;
SET jit = off;
SET client_min_messages = log;
SET debug_print_plan = on;

-- regression: the join-qual Var built for an ANY sublink whose
-- subquery is kept (OFFSET 0) carries varnosyn/varattnosyn (makeVar,
-- makefuncs.c:92-93)
SELECT a FROM b281_t1 WHERE a IN (SELECT x FROM b281_t2 OFFSET 0);

-- regression: an Aggref's relation dependencies are recorded
-- args first, aggfilter last (nodeFuncs.c:3026-3029)
SELECT count('b281_t2'::regclass) FILTER (WHERE a::regclass = 'b281_t1'::regclass) FROM b281_t1;

-- regression: WindowFunc.runCondition is not rewritten by the
-- upper-reference pass (nodeFuncs.c:3056-3064): a generic-plan Param that
-- is also the window sort key stays a Param, not an OUTER Var
SET plan_cache_mode = force_generic_plan;
PREPARE b281_p(int) AS
  SELECT * FROM (SELECT a, row_number() OVER (ORDER BY $1) rn FROM b281_t1) s WHERE rn <= $1;
EXECUTE b281_p(2);
DEALLOCATE b281_p;
RESET plan_cache_mode;

-- regression: a multiply-referenced NOT MATERIALIZED CTE is
-- copied with copyObject (subselect.c:1187), so its scan-qual expressions
-- keep their source locations
WITH c AS NOT MATERIALIZED (SELECT * FROM b281_t1 WHERE a = 1) SELECT * FROM c c1, c c2;

RESET debug_print_plan;
RESET client_min_messages;
RESET jit;
DROP TABLE b281_t1, b281_t2;

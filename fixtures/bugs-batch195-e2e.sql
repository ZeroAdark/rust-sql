-- bugs/batch-195-backend-executor-nodeagg: nodeAgg fixes, expected captured from C 18.6.
\set VERBOSITY verbose
-- regression: ExecBuildAggTrans (execExpr.c:3718) runs a
-- presorted DISTINCT aggregate's transition in program order, so the next
-- aggregate's FILTER already sees its side effects (pgrust ran it after the
-- whole program: currval undefined / the FILTER's error won).
CREATE SEQUENCE b195_s;
CREATE FUNCTION b195_sf(a int, b int) RETURNS int LANGUAGE sql AS $$ SELECT nextval('b195_s')::int + coalesce(a,0) $$;
CREATE AGGREGATE b195_agg(int) (sfunc = b195_sf, stype = int);
EXPLAIN (COSTS OFF) SELECT b195_agg(DISTINCT x), count(*) FILTER (WHERE currval('b195_s') > 0) FROM (VALUES (1),(2),(2),(3)) v(x);
SELECT b195_agg(DISTINCT x), count(*) FILTER (WHERE currval('b195_s') > 0) FROM (VALUES (1),(2),(2),(3)) v(x);
SELECT b195_agg(DISTINCT x), array_agg(currval('b195_s')) FILTER (WHERE currval('b195_s') > 0) FROM (VALUES (1),(2),(2),(3)) v(x);
CREATE FUNCTION b195_err(a int, b int) RETURNS int LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'sfunc ran %', b; END $$;
CREATE AGGREGATE b195_eagg(int) (sfunc = b195_err, stype = int);
SELECT b195_eagg(DISTINCT x), count(*) FILTER (WHERE 1/(x-x) = 1) FROM (VALUES (1),(2)) v(x);
SELECT b195_agg(DISTINCT x ORDER BY x), array_agg(currval('b195_s')) FILTER (WHERE currval('b195_s') > 0) FROM (VALUES (1),(1),(3)) v(x);
-- multi-column presorted DISTINCT takes the same program-order path
SELECT string_agg(DISTINCT x::text, ',' ORDER BY x::text), count(*) FILTER (WHERE x > 1) FROM (VALUES (1),(1),(3),(2)) v(x);
DROP AGGREGATE b195_eagg(int); DROP FUNCTION b195_err(int, int);
DROP AGGREGATE b195_agg(int); DROP FUNCTION b195_sf(int, int); DROP SEQUENCE b195_s;
-- regression / p2#1: the finalize stage seeds every group from
-- INITCOND before combining partials (initialize_aggregate nodeAgg.c:636);
-- the parallel table merge adopted the first partial state as-is.
CREATE TABLE b195_t AS SELECT (i % 4) AS g, (i % 100) AS v FROM generate_series(1, 200000) i;
ANALYZE b195_t;
CREATE AGGREGATE b195_sum100(int) (sfunc = int4pl, stype = int, combinefunc = int4pl, initcond = '100', parallel = safe);
CREATE AGGREGATE b195_max7(int) (sfunc = int4larger, stype = int, combinefunc = int4larger, initcond = '7', parallel = safe);
CREATE AGGREGATE b195_and(bool) (sfunc = booland_statefunc, stype = bool, combinefunc = booland_statefunc, initcond = 'false', parallel = safe);
SET max_parallel_workers_per_gather = 2;
SET parallel_setup_cost = 0;
SET parallel_tuple_cost = 0;
SET min_parallel_table_scan_size = 0;
SET parallel_leader_participation = on;
SET enable_sort = off;
EXPLAIN (COSTS OFF) SELECT g, b195_sum100(v), count(*) FROM b195_t GROUP BY g;
SELECT g, b195_sum100(v) - sum(v) AS extra, b195_max7(v - 1000000), b195_and(true), count(*) FROM b195_t GROUP BY g ORDER BY g;
SELECT g, avg(v), sum(v), count(*) FROM b195_t GROUP BY g ORDER BY g;
-- regression: int4_avg_combine validates both transition
-- arrays (numeric.c:6883); a user aggregate over a SQL sfunc hands the merge
-- short-header or wrong-shape arrays, which take the fmgr combine.
CREATE FUNCTION b195_sf2(s int8[], v int) RETURNS int8[] LANGUAGE sql IMMUTABLE AS $$ SELECT ARRAY[s[1]+1, s[2]+v] $$;
CREATE AGGREGATE b195_avg2(int) (sfunc = b195_sf2, stype = int8[], combinefunc = int4_avg_combine, finalfunc = int8_avg, initcond = '{0,0}', parallel = safe);
CREATE FUNCTION b195_sf3(s int8[], v int) RETURNS int8[] LANGUAGE sql IMMUTABLE AS $$ SELECT ARRAY[s[1]+1, s[2]+v, 0::int8] $$;
CREATE AGGREGATE b195_avg3(int) (sfunc = b195_sf3, stype = int8[], combinefunc = int4_avg_combine, finalfunc = int8_avg, initcond = '{0,0,0}', parallel = safe);
CREATE FUNCTION b195_sfn(s int8[], v int) RETURNS int8[] LANGUAGE sql IMMUTABLE AS $$ SELECT ARRAY[s[1]+1, NULL::int8] $$;
CREATE AGGREGATE b195_avgn(int) (sfunc = b195_sfn, stype = int8[], combinefunc = int4_avg_combine, finalfunc = int8_avg, initcond = '{0,0}', parallel = safe);
EXPLAIN (COSTS OFF) SELECT g, b195_avg2(v) FROM b195_t GROUP BY g;
SELECT g, b195_avg2(v), avg(v) FROM b195_t GROUP BY g ORDER BY g;
SELECT g, b195_avg3(v) FROM b195_t GROUP BY g ORDER BY g;
SELECT g, b195_avgn(v) FROM b195_t GROUP BY g ORDER BY g;
RESET max_parallel_workers_per_gather;
RESET parallel_setup_cost;
RESET parallel_tuple_cost;
RESET min_parallel_table_scan_size;
RESET parallel_leader_participation;
RESET enable_sort;
DROP AGGREGATE b195_avg2(int); DROP FUNCTION b195_sf2(int8[], int);
DROP AGGREGATE b195_avg3(int); DROP FUNCTION b195_sf3(int8[], int);
DROP AGGREGATE b195_avgn(int); DROP FUNCTION b195_sfn(int8[], int);
DROP AGGREGATE b195_sum100(int); DROP AGGREGATE b195_max7(int); DROP AGGREGATE b195_and(bool);
DROP TABLE b195_t;

-- M5-RIG's m5x set (cards/M5.md L0-k and L1 G4; M5-ENTRY-SPI §4.1 R2): plans with user set-returning functions the census
-- corpora lack, run over a fresh initdb by `OUT=crates/gravity/lowerv4/tests/fixtures/m5 scripts/gravity-capture-plans.sh
-- m5x`; every planned statement becomes fixtures/m5/m5x-<nnn>.plan. The fixture headers carry relation facts only, so a
-- crate test that lowers one answers the user functions with its own catalog (tests/v4r233_srf_pl_lang.rs's `Lang`).
\set VERBOSITY terse
\pset pager off
SET client_min_messages = warning;
SET max_parallel_workers_per_gather = 0;
CREATE TABLE t (a int PRIMARY KEY, b int NOT NULL);
INSERT INTO t SELECT i, i % 7 + 1 FROM generate_series(1, 1000) i;
-- plpgsql, VOLATILE: srf.pl in FROM and in the target list (V4R-233: by language); ROWS 3 makes it the hashed side
CREATE FUNCTION three() RETURNS SETOF int LANGUAGE plpgsql ROWS 3 AS $$ BEGIN RETURN NEXT 1; RETURN NEXT 2; RETURN NEXT 3; END $$;
-- SQL, VOLATILE (never inlined): srf.pl
CREATE FUNCTION three_sql() RETURNS SETOF int LANGUAGE sql ROWS 3 AS 'SELECT 1 UNION ALL SELECT 2 UNION ALL SELECT 3';
-- SQL, IMMUTABLE, one SELECT: inlined by the planner (inline_set_returning_function), a subquery and never a FunctionScan
CREATE FUNCTION upto(n int) RETURNS SETOF int LANGUAGE sql IMMUTABLE AS 'SELECT i FROM generate_series(1, n) i';
ANALYZE t;
SET enable_nestloop = off;
SET enable_mergejoin = off;

-- L0-k plain: the plpgsql SRF is the hash join's build side (Hash over the FunctionScan)
SELECT t.a, t.b FROM t JOIN three() f(x) ON t.a = f.x;
-- L0-k fed build: the build side is fed by a pipeline that reads the plpgsql SRF (Hash over an aggregate)
SELECT t.a FROM t JOIN (SELECT DISTINCT x FROM three() f(x)) s ON t.a = s.x;
-- L0-k, SQL body: a non-inlined SQL SRF as the build side
SELECT t.a FROM t JOIN three_sql() f(x) ON t.a = f.x;
-- G4: a library SRF on the build side is srf.function, never srf.pl
SELECT t.a FROM t JOIN generate_series(1, 3) g(x) ON t.a = g.x;
-- G4: an inlined SQL SRF is a subquery, never a FunctionScan
SELECT t.a FROM t JOIN upto(3) u(x) ON t.a = u.x;
-- srf.pl in the target list
SELECT three(), a FROM t WHERE a < 3;
-- srf.pl alone in FROM
SELECT x FROM three() f(x) WHERE x <> 2;

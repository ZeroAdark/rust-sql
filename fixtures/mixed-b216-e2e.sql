-- bugs/batch-216-mixed-reaudit: C-vs-pgrust parity for the single-session
-- fixes. Expected file captured from C 18.6 (scripts/regress-diff.sh --capture).
\set VERBOSITY verbose

-- execexpr domain engine: a constraint reload (pg_constraint invalidation)
-- whose constant folding re-enters the engine through domain_in
CREATE DOMAIN b216_d2 AS int CHECK (VALUE > 0);
CREATE FUNCTION b216_f0(t text) RETURNS int IMMUTABLE LANGUAGE plpgsql AS $$ BEGIN PERFORM t::b216_d2[]; RETURN 0; END $$;
CREATE DOMAIN b216_d AS int CHECK (VALUE > b216_f0('{1}'));
SELECT '{5}'::text::b216_d[];
CREATE TABLE b216_junk (x int CHECK (x > 0));
SELECT '{5}'::text::b216_d[];
SELECT '{0}'::text::b216_d[];
DROP TABLE b216_junk;
DROP DOMAIN b216_d;
DROP FUNCTION b216_f0;
DROP DOMAIN b216_d2;

-- sql_functions cache: a parked parallel worker's compiled entries die with
-- its plancache; the next task recompiles instead of reusing dead plans
CREATE TABLE b216_big AS SELECT g AS x FROM generate_series(1,100000) g;
ANALYZE b216_big;
CREATE FUNCTION b216_sf(i int) RETURNS int LANGUAGE sql PARALLEL SAFE STABLE AS 'SELECT i; SELECT i + 1';
SET max_parallel_workers_per_gather = 2;
SET parallel_setup_cost = 0;
SET parallel_tuple_cost = 0;
SET min_parallel_table_scan_size = 0;
SELECT count(*) FROM b216_big WHERE b216_sf(x) > 0;
SELECT count(*) FROM b216_big WHERE b216_sf(x) > 0;
SELECT count(*) FROM b216_big WHERE b216_sf(x) > 0;
SELECT count(*) FROM b216_big WHERE b216_sf(x) > 0;
RESET max_parallel_workers_per_gather;
RESET parallel_setup_cost;
RESET parallel_tuple_cost;
RESET min_parallel_table_scan_size;
DROP TABLE b216_big;
DROP FUNCTION b216_sf;

-- dfmgr: a module entry point has no pg_finfo record, so it is not SQL-callable
CREATE FUNCTION b216_plug(bigint) RETURNS void AS '$libdir/pgoutput', '_PG_output_plugin_init' LANGUAGE C;
CREATE FUNCTION b216_plug0() RETURNS void AS '$libdir/test_decoding', '_PG_output_plugin_init' LANGUAGE C;
SELECT count(*) FROM pg_proc WHERE proname LIKE 'b216_plug%';

-- plpgsql: a dropped function reached through the same FmgrInfo reports
-- funccache.c's elog (XX000), not 42883
CREATE FUNCTION b216_g(i int) RETURNS int LANGUAGE plpgsql AS $$ BEGIN IF i = 1 THEN DROP FUNCTION b216_g(int); END IF; RETURN i; END $$;
DO $$ BEGIN
  PERFORM b216_g(x) FROM generate_series(1,3) x;
EXCEPTION WHEN OTHERS THEN
  RAISE NOTICE 'sqlstate=% message=%', SQLSTATE, regexp_replace(SQLERRM, '\d+', 'N');
END $$;
DROP FUNCTION IF EXISTS b216_g(int);

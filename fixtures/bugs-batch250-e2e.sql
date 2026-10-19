-- Differential fixture for the 2026-09-13 audit bug batch 250 (deferred
-- rows re-verified: dfmgr's pg_finfo_ record gate, WindowAgg duplicate
-- window-function dedup). Run via scripts/bugs-batch250-e2e.sh against the
-- frozen expected captured from C PostgreSQL 18.6.
\set VERBOSITY verbose
\pset pager off

-- regression: a symbol the library exports without a pg_finfo_ record
-- (the magic block function, _PG_init, a pg_finfo_ record itself) is
-- fetch_finfo_record's 42883 with its HINT, not load_external_function's.
CREATE FUNCTION b250_m() RETURNS void AS '$libdir/cube', 'Pg_magic_func' LANGUAGE C;
CREATE FUNCTION b250_i() RETURNS void AS '$libdir/plpgsql', '_PG_init' LANGUAGE C;
CREATE FUNCTION b250_fi() RETURNS void AS '$libdir/cube', 'pg_finfo_cube_in' LANGUAGE C;

-- regression / #6: a nested statement prepared through SPI
-- (plpgsql) or compiled as a SQL-function body dumps its analyzed and
-- rewritten trees under debug_print_parse / debug_print_rewritten
-- (postgres.c:803/:870 via pg_analyze_and_rewrite_withcb); the proconfig
-- scopes the switches to the nested statements only.
SET client_min_messages = log;
CREATE FUNCTION b250_dsql(int) RETURNS int LANGUAGE sql
  SET debug_print_parse = on SET debug_print_rewritten = on
  AS 'SELECT $1 + 1';
SELECT b250_dsql(1);
CREATE FUNCTION b250_dpl(int) RETURNS int LANGUAGE plpgsql
  SET debug_print_parse = on SET debug_print_rewritten = on
  AS $$ DECLARE r int; BEGIN SELECT $1 + 2 INTO r; RETURN r; END $$;
SELECT b250_dpl(1);
RESET client_min_messages;
DROP FUNCTION b250_dsql(int);
DROP FUNCTION b250_dpl(int);

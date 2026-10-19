-- Differential fixture for the 2026-09-13 audit bug batch 111
-- (backend/commands/functioncmds: cursor positions for parameter typmod and
-- %TYPE errors, COST/ROWS double rounding, language aliases over a ported
-- handler, CALL argument constant folding). Run via
-- scripts/bugs-batch111-e2e.sh against the frozen expected captured from C
-- PostgreSQL 18.6.
\set VERBOSITY verbose
\pset pager off
-- regression / regression / regression:
-- typenameTypeMod errors carry the parameter's TypeName position
CREATE FUNCTION b111_typmod_f(x pg_catalog.int4(2)) RETURNS integer LANGUAGE sql AS 'SELECT 1';
CREATE FUNCTION b111_typmod_g(x varchar(0)) RETURNS integer LANGUAGE sql AS 'SELECT 1';
CREATE FUNCTION b111_typmod_h(x integer, y varchar(0)) RETURNS varchar(0) LANGUAGE sql AS 'SELECT 1';
CREATE FUNCTION b111_typmod_i(x integer) RETURNS varchar(0) LANGUAGE sql AS 'SELECT 1';
CREATE AGGREGATE b111_agg(pg_catalog.int4(2)) (SFUNC = int4pl, STYPE = int4);
CREATE AGGREGATE b111_agg(integer ORDER BY varchar(0)) (SFUNC = int4pl, STYPE = int4);
-- regression / regression / regression:
-- the %TYPE arm's errors carry the TypeName position
CREATE TABLE b111_pct(a integer, b text);
CREATE FUNCTION b111_pct_f(x b111_pct.missing%TYPE) RETURNS integer LANGUAGE sql AS 'SELECT 1';
CREATE FUNCTION b111_pct_f(x b111_pct.a%TYPE, y b111_pct.missing%TYPE) RETURNS integer LANGUAGE sql AS 'SELECT 1';
CREATE FUNCTION b111_pct_f(x b111_pct.a%TYPE) RETURNS b111_pct.missing%TYPE LANGUAGE sql AS 'SELECT 1';
CREATE FUNCTION b111_pct_f(x a.b.c.b111_pct.a%TYPE) RETURNS integer LANGUAGE sql AS 'SELECT 1';
CREATE FUNCTION b111_pct_f(x b111_nosuch.a%TYPE) RETURNS integer LANGUAGE sql AS 'SELECT 1';
CREATE FUNCTION b111_pct_f(x b111_pct.a%TYPE, y public.b111_pct.b%TYPE) RETURNS integer LANGUAGE sql AS 'SELECT 1';
SELECT pg_get_function_arguments('b111_pct_f'::regproc);
CREATE AGGREGATE b111_agg(b111_pct.missing%TYPE) (SFUNC = int4pl, STYPE = int4);
CREATE AGGREGATE b111_agg(x.y.z.b111_pct.a%TYPE) (SFUNC = int4pl, STYPE = int4);
CREATE AGGREGATE b111_agg(b111_pct.a%TYPE) (SFUNC = int4pl, STYPE = int4);
-- regression / regression: defGetNumeric goes through
-- double before the float4 store (double rounding at the float4 midpoint)
CREATE FUNCTION b111_cost() RETURNS integer LANGUAGE sql AS 'SELECT 1' COST 1.0000000596046448;
SELECT procost, procost > 1 FROM pg_proc WHERE oid = 'b111_cost()'::regprocedure;
CREATE FUNCTION b111_cost2() RETURNS SETOF integer LANGUAGE sql AS 'SELECT 1'
  COST 1.00000005960464477539062500000000001 ROWS 1000.00006103515625000000000001;
SELECT procost, procost > 1, prorows, prorows > 1000 FROM pg_proc WHERE oid = 'b111_cost2()'::regprocedure;
ALTER FUNCTION b111_cost2() COST 3.0000001192092896 ROWS 3.0000001192092896;
SELECT procost, procost > 3, prorows, prorows > 3 FROM pg_proc WHERE oid = 'b111_cost2()'::regprocedure;
ALTER FUNCTION b111_cost2() COST 3.00000011920928955078125000001 ROWS 25;
SELECT procost, procost > 3, prorows FROM pg_proc WHERE oid = 'b111_cost2()'::regprocedure;
CREATE FUNCTION b111_cost3() RETURNS integer LANGUAGE sql AS 'SELECT 1' COST 0.5 ROWS 1;
CREATE FUNCTION b111_cost3() RETURNS integer LANGUAGE sql AS 'SELECT 1' COST 0;
CREATE FUNCTION b111_cost3() RETURNS integer LANGUAGE sql AS 'SELECT 1' SET b111_nonexistent TO 'x' COST 0;
-- regression: a language alias whose call handler is the
-- ported plpgsql handler is usable
CREATE LANGUAGE b111_plalias HANDLER plpgsql_call_handler;
CREATE FUNCTION b111_alias_f(i integer) RETURNS integer LANGUAGE b111_plalias AS 'BEGIN RETURN i + 1; END';
SELECT b111_alias_f(41);
SELECT lanname FROM pg_language l JOIN pg_proc p ON p.prolang = l.oid WHERE p.proname = 'b111_alias_f';
CREATE PROCEDURE b111_alias_p(INOUT i integer) LANGUAGE b111_plalias AS 'BEGIN i := i * 2; END';
CALL b111_alias_p(21);
DROP LANGUAGE b111_plalias CASCADE;
-- regression: CALL arguments are planned (constant-folded)
-- before evaluation, like ExecPrepareExpr
CREATE PROCEDURE b111_call_p(integer) LANGUAGE plpgsql AS 'BEGIN RAISE NOTICE ''arg %'', $1; END';
CALL b111_call_p(CASE WHEN random() >= 0 THEN 1 ELSE 1 / 0 END);
CALL b111_call_p(CASE WHEN random() >= 0 THEN 1 ELSE 2 END);
CALL b111_call_p(1 + 1);
CALL b111_call_p(coalesce(NULL::integer, 7));
DO $$ BEGIN CALL b111_call_p(CASE WHEN random() >= 0 THEN 1 ELSE 1 / 0 END); END $$;
BEGIN;
CALL b111_call_p(CASE WHEN random() >= 0 THEN 3 ELSE 1 / 0 END);
ROLLBACK;
DROP PROCEDURE b111_call_p(integer);
DROP FUNCTION b111_cost(), b111_cost2(), b111_pct_f(integer, text);
DROP AGGREGATE b111_agg(integer);
DROP TABLE b111_pct;

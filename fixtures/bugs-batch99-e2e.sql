-- Differential fixture for the 2026-09-13 audit bug batch 99
-- (backend/executor/execexpr). Run via scripts/bugs-batch99-e2e.sh against
-- the frozen expected captured from C PostgreSQL 18.6.
\pset pager off
-- regression / regression: pgstat_init_function_usage
-- re-reads track_functions on every call, so a set_config() argument that
-- turns tracking off stops the enclosing call from being counted.
CREATE FUNCTION b99_f(text) RETURNS integer LANGUAGE plpgsql AS $$ BEGIN RETURN 1; END $$;
CREATE FUNCTION b99_g(integer) RETURNS integer LANGUAGE plpgsql AS $$ BEGIN RETURN $1; END $$;
SET track_functions = 'all';
BEGIN;
SELECT b99_f(set_config('track_functions', 'none', false));
SELECT pg_stat_get_xact_function_calls('b99_f(text)'::regprocedure);
COMMIT;
SET track_functions = 'all';
BEGIN;
SELECT set_config('track_functions', 'none', false), b99_g(i) FROM generate_series(1, 3) AS g(i);
SELECT pg_stat_get_xact_function_calls('b99_g(integer)'::regprocedure);
COMMIT;
SET track_functions = 'all';
BEGIN;
SELECT b99_g(i) FROM generate_series(1, 3) AS g(i);
SELECT pg_stat_get_xact_function_calls('b99_g(integer)'::regprocedure);
COMMIT;
RESET track_functions;
-- regression: slice assignment into a fixed-length array type is
-- rejected before the replacement value is read as a varlena.
CREATE TABLE b99_raw_slice (n name);
INSERT INTO b99_raw_slice VALUES ('a');
UPDATE b99_raw_slice SET n[1:2] = 'b'::name;
SELECT ('b'::name)[1:2];
-- regression: an anonymous ROW() descriptor records each
-- field's collation (ExecTypeFromExprList).
CREATE TABLE b99_coll (v text);
INSERT INTO b99_coll VALUES ('a');
SELECT pg_catalog.record_lt(ROW(v COLLATE "C"), ROW(v COLLATE "POSIX")) FROM b99_coll;
SELECT pg_catalog.record_eq(ROW(v COLLATE "C"), ROW(v COLLATE "C")) FROM b99_coll;
-- regression: 65536 PASSING variables (the count was a u16).
\set ECHO none
SELECT format('SELECT JSON_VALUE(''{}''::jsonb, ''$v65535'' PASSING %s RETURNING integer ERROR ON ERROR) AS v65535',
              string_agg(format('%s AS v%s', i, i), ', ' ORDER BY i))
FROM generate_series(0, 65535) i \gexec
\set ECHO all
-- regression: array_map checks the cumulative result size after
-- each element (54000 after two 512MB elements, before the third is cast).
CREATE TYPE b99_huge AS (n int);
CREATE FUNCTION b99_huge_text(x b99_huge) RETURNS text LANGUAGE plpgsql AS $$
BEGIN
  IF x.n = 3 THEN RAISE EXCEPTION 'third element evaluated'; END IF;
  RETURN repeat('x', 536870912);
END $$;
CREATE CAST (b99_huge AS text) WITH FUNCTION b99_huge_text(b99_huge);
CREATE TABLE b99_huge_t AS SELECT ARRAY[ROW(1)::b99_huge, ROW(2)::b99_huge, ROW(3)::b99_huge] AS a;
SELECT length(a::text[]::text) FROM b99_huge_t;
DROP TABLE b99_huge_t;
DROP CAST (b99_huge AS text);
DROP FUNCTION b99_huge_text(b99_huge);
DROP TYPE b99_huge;
DROP TABLE b99_coll;
DROP TABLE b99_raw_slice;
DROP FUNCTION b99_f(text);
DROP FUNCTION b99_g(integer);

-- bugs/batch-198-backend-parser: parser fixes, expected captured from C 18.6.
\set VERBOSITY verbose
\set SHOW_CONTEXT always
-- regression: a scalar subquery whose output collation is
-- indeterminate merges as InvalidOid (parse_collate.c:521), which the sort-
-- target conflict message prints as "(null)".
CREATE TABLE b198_c (c1 text COLLATE "C", c2 text COLLATE "POSIX");
SELECT (SELECT c1 || c2 FROM b198_c LIMIT 1) || c1 FROM b198_c ORDER BY 1;
SELECT c1 || (SELECT c1 || c2 FROM b198_c LIMIT 1) FROM b198_c ORDER BY 1;
DROP TABLE b198_c;
-- regression: a WindowClause is zero-initialised
-- (parse_clause.c:2830), so inRangeAsc reads false unless a RANGE offset
-- frame sets it.
SET debug_print_parse = on;
SET client_min_messages = log;
SELECT count(*) OVER () FROM (SELECT 1) s;
SELECT count(*) OVER (ORDER BY g RANGE BETWEEN 1 PRECEDING AND CURRENT ROW) FROM (SELECT 1 AS g) s;
RESET client_min_messages;
RESET debug_print_parse;
-- regression: plpgsql_ns_lookup checks each namespace level in
-- turn (pl_funcs.c:156): a block label at a nearer level wins over an outer
-- unassigned record of the same name, and the reverse nesting still errors.
CREATE FUNCTION b198_lbl_inner() RETURNS int AS $$
DECLARE r record; y int;
BEGIN
  <<r>>
  DECLARE x int := 42;
  BEGIN
    y := r.x;
    SELECT r.x + 1 INTO y;
    RAISE NOTICE 'r.x = %, y = %', r.x, y;
    RETURN r.x + 0;
  END;
END $$ LANGUAGE plpgsql;
SELECT b198_lbl_inner();
CREATE FUNCTION b198_lbl_outer() RETURNS int AS $$
<<r>>
DECLARE x int := 42;
BEGIN
  DECLARE r record; y int;
  BEGIN
    y := r.x;
    RETURN y;
  END;
END $$ LANGUAGE plpgsql;
SELECT b198_lbl_outer();
CREATE FUNCTION b198_lbl_nofield() RETURNS int AS $$
DECLARE r record; y int;
BEGIN
  <<r>>
  DECLARE x int := 42;
  BEGIN
    y := r.z;
    RETURN y;
  END;
END $$ LANGUAGE plpgsql;
SELECT b198_lbl_nofield();
DROP FUNCTION b198_lbl_inner(), b198_lbl_outer(), b198_lbl_nofield();
-- regression / regression: stringTypeDatum goes
-- through InputFunctionCall, whose NULL-result check says "input function
-- %u returned NULL" (fmgr.c:1560). The OID is masked (it varies per run).
CREATE TYPE b198_t;
CREATE FUNCTION b198_in(cstring) RETURNS b198_t AS 'pg_get_function_arguments' LANGUAGE internal IMMUTABLE;
CREATE FUNCTION b198_out(b198_t) RETURNS cstring AS 'int4out' LANGUAGE internal STRICT IMMUTABLE;
CREATE TYPE b198_t (INPUT = b198_in, OUTPUT = b198_out, INTERNALLENGTH = 4, PASSEDBYVALUE);
DO $$ BEGIN PERFORM 'abc'::b198_t; EXCEPTION WHEN OTHERS THEN RAISE NOTICE '% %', SQLSTATE, regexp_replace(SQLERRM, '\d+', 'N'); END $$;
DO $$ BEGIN PERFORM b198_t 'abc'; EXCEPTION WHEN OTHERS THEN RAISE NOTICE '% %', SQLSTATE, regexp_replace(SQLERRM, '\d+', 'N'); END $$;
SELECT NULL::b198_t;
DROP TYPE b198_t CASCADE;
-- regression: pts_error_callback stays installed across the
-- whole raw parse (parse_type.c:751), so the identifier-truncation NOTICE
-- carries the "invalid type name" CONTEXT too.
SELECT to_regtype(repeat('x', 70));
SELECT to_regtype('int4 ' || repeat('x', 70));
-- regression: the byte <xeu>. matched after a high surrogate goes into
-- the message verbatim (scan.l:1240).
SELECT E'\uD83Dé';
SELECT E'\uD83Dab';

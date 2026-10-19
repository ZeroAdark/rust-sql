-- bugs/batch-214-backend-utils-adt: utils/adt fixes, expected captured from C 18.6.
\set VERBOSITY verbose
-- regression: a hypothetical-set final called on an already
-- sorted (shared, FINALFUNC_MODIFY = READ_ONLY) transition state raises
-- tuplesort.c:1312 "invalid tuplesort state"; pgrust panicked on an assert.
CREATE AGGREGATE b214_rank(VARIADIC "any" ORDER BY VARIADIC "any") (SFUNC = ordered_set_transition_multi, STYPE = internal, FINALFUNC = rank_final, FINALFUNC_EXTRA, HYPOTHETICAL, FINALFUNC_MODIFY = READ_ONLY);
CREATE AGGREGATE b214_dense_rank(VARIADIC "any" ORDER BY VARIADIC "any") (SFUNC = ordered_set_transition_multi, STYPE = internal, FINALFUNC = dense_rank_final, FINALFUNC_EXTRA, HYPOTHETICAL, FINALFUNC_MODIFY = READ_ONLY);
CREATE AGGREGATE b214_percent_rank(VARIADIC "any" ORDER BY VARIADIC "any") (SFUNC = ordered_set_transition_multi, STYPE = internal, FINALFUNC = percent_rank_final, FINALFUNC_EXTRA, HYPOTHETICAL, FINALFUNC_MODIFY = READ_ONLY);
CREATE AGGREGATE b214_cume_dist(VARIADIC "any" ORDER BY VARIADIC "any") (SFUNC = ordered_set_transition_multi, STYPE = internal, FINALFUNC = cume_dist_final, FINALFUNC_EXTRA, HYPOTHETICAL, FINALFUNC_MODIFY = READ_ONLY);
SELECT b214_rank(2) WITHIN GROUP (ORDER BY x), b214_dense_rank(3) WITHIN GROUP (ORDER BY x) FROM (VALUES (1),(4),(4)) v(x);
SELECT b214_rank(2) WITHIN GROUP (ORDER BY x), b214_rank(3) WITHIN GROUP (ORDER BY x) FROM (VALUES (1),(4)) v(x);
SELECT b214_dense_rank(2) WITHIN GROUP (ORDER BY x), b214_dense_rank(3) WITHIN GROUP (ORDER BY x) FROM (VALUES (1),(4)) v(x);
SELECT b214_percent_rank(2) WITHIN GROUP (ORDER BY x), b214_percent_rank(3) WITHIN GROUP (ORDER BY x) FROM (VALUES (1),(4)) v(x);
SELECT b214_cume_dist(2) WITHIN GROUP (ORDER BY x), b214_cume_dist(3) WITHIN GROUP (ORDER BY x) FROM (VALUES (1),(4)) v(x);
DROP AGGREGATE b214_rank(VARIADIC "any" ORDER BY VARIADIC "any");
DROP AGGREGATE b214_dense_rank(VARIADIC "any" ORDER BY VARIADIC "any");
DROP AGGREGATE b214_percent_rank(VARIADIC "any" ORDER BY VARIADIC "any");
DROP AGGREGATE b214_cume_dist(VARIADIC "any" ORDER BY VARIADIC "any");
-- regression: mode() compares adjacent values through
-- FunctionCall2Coll (orderedsetaggs.c:1094); an equality function that
-- returns SQL NULL raises fmgr.c "function %u returned NULL" (XX000).
CREATE TYPE b214_m;
CREATE FUNCTION b214_m_in(cstring) RETURNS b214_m LANGUAGE internal IMMUTABLE STRICT AS 'int4in';
CREATE FUNCTION b214_m_out(b214_m) RETURNS cstring LANGUAGE internal IMMUTABLE STRICT AS 'int4out';
CREATE TYPE b214_m (INPUT = b214_m_in, OUTPUT = b214_m_out, LIKE = int4);
CREATE FUNCTION b214_m_eq(b214_m, b214_m) RETURNS bool LANGUAGE sql IMMUTABLE AS 'SELECT NULL::bool';
CREATE FUNCTION b214_m_lt(b214_m, b214_m) RETURNS bool LANGUAGE internal IMMUTABLE STRICT AS 'int4lt';
CREATE FUNCTION b214_m_cmp(b214_m, b214_m) RETURNS int LANGUAGE internal IMMUTABLE STRICT AS 'btint4cmp';
CREATE OPERATOR = (LEFTARG = b214_m, RIGHTARG = b214_m, FUNCTION = b214_m_eq);
CREATE OPERATOR < (LEFTARG = b214_m, RIGHTARG = b214_m, FUNCTION = b214_m_lt);
CREATE OPERATOR CLASS b214_m_ops DEFAULT FOR TYPE b214_m USING btree AS OPERATOR 1 <, OPERATOR 3 =, FUNCTION 1 b214_m_cmp(b214_m, b214_m);
DO $$ BEGIN PERFORM mode() WITHIN GROUP (ORDER BY v) FROM (VALUES ('1'::b214_m), ('1'), ('2')) s(v); EXCEPTION WHEN OTHERS THEN RAISE NOTICE '% %', SQLSTATE, regexp_replace(SQLERRM, '\d+', 'N'); END $$;
SELECT mode() WITHIN GROUP (ORDER BY v) FROM (VALUES ('1'::b214_m)) s(v);
DROP TYPE b214_m CASCADE;
-- regression: ri_GenerateQual's add_cast_to spells a temp-schema
-- type as ::pg_temp.typname (ruleutils.c:13523 get_namespace_name_or_temp);
-- the RI query text surfaces in the SPI error CONTEXT.
CREATE TYPE pg_temp.b214_t;
CREATE FUNCTION b214_t_in(cstring) RETURNS pg_temp.b214_t LANGUAGE internal IMMUTABLE STRICT AS 'int4in';
CREATE FUNCTION b214_t_out(pg_temp.b214_t) RETURNS cstring LANGUAGE internal IMMUTABLE STRICT AS 'int4out';
CREATE TYPE pg_temp.b214_t (INPUT = b214_t_in, OUTPUT = b214_t_out, LIKE = int4);
CREATE FUNCTION b214_t_eq(pg_temp.b214_t, pg_temp.b214_t) RETURNS bool LANGUAGE plpgsql IMMUTABLE AS $$ BEGIN RAISE EXCEPTION 'b214 eq called'; END $$;
CREATE FUNCTION b214_t_lt(pg_temp.b214_t, pg_temp.b214_t) RETURNS bool LANGUAGE internal IMMUTABLE STRICT AS 'int4lt';
CREATE FUNCTION b214_t_le(pg_temp.b214_t, pg_temp.b214_t) RETURNS bool LANGUAGE internal IMMUTABLE STRICT AS 'int4le';
CREATE FUNCTION b214_t_gt(pg_temp.b214_t, pg_temp.b214_t) RETURNS bool LANGUAGE internal IMMUTABLE STRICT AS 'int4gt';
CREATE FUNCTION b214_t_ge(pg_temp.b214_t, pg_temp.b214_t) RETURNS bool LANGUAGE internal IMMUTABLE STRICT AS 'int4ge';
CREATE FUNCTION b214_t_cmp(pg_temp.b214_t, pg_temp.b214_t) RETURNS int LANGUAGE plpgsql IMMUTABLE AS $$ BEGIN RAISE EXCEPTION 'b214 cmp called'; END $$;
CREATE OPERATOR = (LEFTARG = pg_temp.b214_t, RIGHTARG = pg_temp.b214_t, FUNCTION = b214_t_eq);
CREATE OPERATOR < (LEFTARG = pg_temp.b214_t, RIGHTARG = pg_temp.b214_t, FUNCTION = b214_t_lt);
CREATE OPERATOR <= (LEFTARG = pg_temp.b214_t, RIGHTARG = pg_temp.b214_t, FUNCTION = b214_t_le);
CREATE OPERATOR > (LEFTARG = pg_temp.b214_t, RIGHTARG = pg_temp.b214_t, FUNCTION = b214_t_gt);
CREATE OPERATOR >= (LEFTARG = pg_temp.b214_t, RIGHTARG = pg_temp.b214_t, FUNCTION = b214_t_ge);
CREATE OPERATOR CLASS b214_t_ops DEFAULT FOR TYPE pg_temp.b214_t USING btree AS OPERATOR 1 <, OPERATOR 2 <=, OPERATOR 3 =, OPERATOR 4 >=, OPERATOR 5 >, FUNCTION 1 b214_t_cmp(pg_temp.b214_t, pg_temp.b214_t);
CREATE DOMAIN pg_temp.b214_d AS pg_temp.b214_t;
CREATE TABLE b214_pk (k pg_temp.b214_t PRIMARY KEY);
CREATE TABLE b214_fk (k pg_temp.b214_d REFERENCES b214_pk);
INSERT INTO b214_pk VALUES ('1');
DO $$ DECLARE ctx text; BEGIN INSERT INTO b214_fk VALUES ('1'); EXCEPTION WHEN OTHERS THEN GET STACKED DIAGNOSTICS ctx = PG_EXCEPTION_CONTEXT; RAISE NOTICE '% / cast target schema: %', SQLERRM, (regexp_match(ctx, '\$1::"?(pg_temp[_0-9]*)"?'))[1]; END $$;
DROP TABLE b214_fk, b214_pk;
DROP TYPE pg_temp.b214_t CASCADE;
-- regression: numeric_in quotes the raw input bytes (numeric.c:805
-- %s of the cstring); pgrust substituted U+FFFD for non-UTF-8 bytes.
CREATE DATABASE b214_ascii ENCODING 'SQL_ASCII' TEMPLATE template0;
\c b214_ascii
\set VERBOSITY verbose
SELECT convert_from(decode('3132e9', 'hex'), 'SQL_ASCII')::numeric;
SELECT ('12' || chr(233))::numeric;
SELECT pg_input_error_info(convert_from(decode('e92037', 'hex'), 'SQL_ASCII'), 'numeric');
\c postgres
DROP DATABASE b214_ascii;

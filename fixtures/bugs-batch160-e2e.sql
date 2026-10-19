-- Two-binary differential corpus for the 2026-09-13 bug-inventory batch
-- batch-160-backend-utils-adt (C 18.6 oracle vs pgrust). Each leg is one row
-- of the batch (utils/adt: tsginidx compatibility stubs, hex_decode_safe soft
-- errors, to_char/to_number NUM edge cases, geo typinput raw bytes,
-- pg_stat_file's own result descriptor).
\set VERBOSITY verbose
CREATE DATABASE b160e2e TEMPLATE template0 ENCODING 'UTF8';
\c b160e2e
\set VERBOSITY verbose
-- byteain alias: raw (non-UTF-8) bytes into a text value.
CREATE FUNCTION b160_bi(cstring) RETURNS text AS 'byteain' LANGUAGE internal;
-- regression: the pre-9.1 GIN compatibility stubs check PG_NARGS first
-- (tsginidx.c:307/319/331) when reached through an internal-language alias.
CREATE FUNCTION b160_x2(tsvector, int) RETURNS int AS 'gin_extract_tsvector_2args' LANGUAGE internal;
SELECT b160_x2('a'::tsvector, 1);
CREATE FUNCTION b160_q5(tsquery, int) RETURNS int AS 'gin_extract_tsquery_5args' LANGUAGE internal;
SELECT b160_q5('a'::tsquery, 1);
CREATE FUNCTION b160_c6(tsquery, int) RETURNS int AS 'gin_tsquery_consistent_6args' LANGUAGE internal;
SELECT b160_c6('a'::tsquery, 1);
-- regression: hex_decode_safe's ereturn (encode.c:236) skips the
-- pg_mblen_range message argument when no details are wanted.
SELECT pg_input_is_valid('\x' || b160_bi('\xe2'), 'bytea');
SELECT pg_input_is_valid('\x1' || b160_bi('\xe2'), 'bytea');
SELECT pg_input_is_valid('\xzz', 'bytea');
SELECT * FROM pg_input_error_info('\x' || b160_bi('\xe2'), 'bytea');
SELECT ('\x' || b160_bi('\xe2'))::bytea;
-- regression: numeric_to_char computes 10^multi before the
-- NaN/Infinity split (formatting.c:6598).
SELECT to_char('NaN'::numeric, '9V' || repeat('9', 131073));
SELECT to_char('Infinity'::numeric, '9V' || repeat('9', 131073));
SELECT to_char('-Infinity'::numeric, '9V' || repeat('9', 131073));
SELECT to_char('NaN'::numeric, '9V99');
SELECT to_char('1.5'::numeric, '9V99');
-- regression: NUM_processor's pg_mblen_range (formatting.c:5941,
-- 6391) raises 22021 on a truncated multibyte input character.
SELECT to_number('1' || b160_bi('\xe2'), '9' || b160_bi('\xe2') || '9');
SELECT to_number(b160_bi('\xe2') || '1', 'L9');
SELECT to_number('1' || b160_bi('\xe2'), '9L');
SELECT to_number('1' || b160_bi('\xe2'), '9');
SELECT to_number('1' || b160_bi('\xc3a9') || '2', '9' || b160_bi('\xc3a9') || '9');
-- Same mechanism on the format parser (pg_mblen_cstr, formatting.c:1479/1497)
-- and DCH_from_char's input walk (formatting.c:3352-3378).
SELECT to_char(1, '9' || b160_bi('\xe2') || '9');
SELECT to_char(1, '9' || b160_bi('\xe2'));
SELECT to_timestamp('2020' || b160_bi('\xe2'), 'YYYY' || b160_bi('\xe2'));
SELECT to_timestamp('2020' || b160_bi('\xe2'), 'YYYYX');
SELECT to_timestamp('2020' || b160_bi('\xe2') || '1', 'YYYYX');
SELECT to_date('2020' || b160_bi('\xe2'), 'YYYY"' || b160_bi('\xe2') || '"');
SELECT to_timestamp('01' || b160_bi('\xe2'), 'DDth');
SELECT to_char(now(), 'YYYY' || b160_bi('\xe2') || '9');
-- regression: geometric typinputs take the cstring's raw bytes and
-- echo them in the 22P02 message (geo_ops.c:1837 point_in ... pair_decode).
SELECT b160_bi('\xff')::point;
SELECT pg_input_is_valid(b160_bi('\xff'), 'point');
SELECT * FROM pg_input_error_info(b160_bi('\xff'), 'point');
SELECT ('(1,' || b160_bi('\xff') || ')')::point;
SELECT ('(1e400,' || b160_bi('\xff') || ')')::point;
SELECT b160_bi('\xff')::box;
SELECT b160_bi('\xff')::path;
SELECT b160_bi('\xff')::polygon;
SELECT b160_bi('\xff')::lseg;
SELECT ('{1,2,' || b160_bi('\xff') || '}')::line;
SELECT b160_bi('\xff')::circle;
SELECT ('(1,1)' || b160_bi('\xc3'))::point;
-- regression: pg_stat_file builds and blesses its own six-column
-- descriptor (genfile.c:444), so a RECORD-returning alias works.
CREATE FUNCTION b160_sf(text) RETURNS record AS 'pg_stat_file' LANGUAGE internal;
SELECT size, isdir, creation FROM b160_sf('PG_VERSION') AS (size bigint, access timestamptz, modification timestamptz, change timestamptz, creation timestamptz, isdir bool);
CREATE FUNCTION b160_sf2(text, boolean) RETURNS record AS 'pg_stat_file' LANGUAGE internal;
SELECT size, isdir FROM b160_sf2('PG_VERSION', true) AS (size bigint, access timestamptz, modification timestamptz, change timestamptz, creation timestamptz, isdir bool);
SELECT b160_sf2('no_such_file_b160', true) IS NULL;
SELECT size, isdir FROM pg_stat_file('PG_VERSION');

-- 2026-09-13 bug campaign, batch-166 (backend/utils/adt): C echoes raw input
-- bytes in its messages where pgrust used to send U+FFFD; bool_accum_inv's
-- NULL-state elog; format_type_be(DATEOID) under a shadowing "date" type;
-- %.*s NUL truncation in the encode/hex messages. Expected captured from C
-- PostgreSQL 18.6. bi() smuggles arbitrary bytes into text via byteain.
\set VERBOSITY verbose
CREATE FUNCTION bi(cstring) RETURNS text AS 'byteain' LANGUAGE internal IMMUTABLE STRICT;
-- regression: NULL state reported before the aggregate-context check
CREATE FUNCTION bai166(int8, boolean) RETURNS int8 AS 'bool_accum_inv' LANGUAGE internal;
SELECT bai166(NULL, true);
-- regression: date/time/timetz input errors quote the raw bytes
SELECT bi('2020-01-\377')::date;
SELECT bi('12:\377')::time;
SELECT bi('12:\377')::timetz;
SELECT bi('2020-13-01\377')::date;
SELECT bi('5874898-01-01\377')::date;
SELECT pg_input_error_info(bi('2020-01-\377'), 'date');
SELECT pg_input_is_valid(bi('2020-01-\377'), 'time');
-- regression: extract() names the type through format_type_be
CREATE SCHEMA s166;
CREATE DOMAIN s166.date AS int;
SET search_path = s166, pg_catalog;
SELECT extract(foo from '2020-01-01'::pg_catalog.date);
SELECT extract(timezone from '2020-01-01'::pg_catalog.date);
SELECT extract(day from '2020-01-01'::pg_catalog.date);
RESET search_path;
SELECT extract(foo from '2020-01-01'::date);
-- regression
SELECT pg_size_bytes(bi('1\377'));
SELECT pg_size_bytes(bi('1 x\377'));
SELECT pg_size_bytes(bi(' 1.5 kB '));
-- regression: %.*s stops at the NUL
SELECT decode(bi('ab\000'), 'base64');
SELECT decode(bi('a\000'), 'hex');
SELECT pg_input_error_info(bi('a\000'), 'bytea');
-- regression
SELECT bi('1.5\377')::float8;
SELECT bi('1.5\377')::float4;
SELECT pg_input_error_info(bi('1.5\377'), 'float8');
-- regression / -p2#1
SELECT to_date(bi('\377'), 'YYYY');
SELECT to_date(bi('\377oo 2020'), 'Mon YYYY');
SELECT to_timestamp(bi('x\377'), 'HH24');
-- regression
SELECT jsonb_path_query('{"a":1}', ('strict $.' || bi('k\377'))::jsonpath);
SELECT jsonb_path_query('{"a":1}', ('strict $."' || bi('k\377') || '"')::jsonpath);
SELECT jsonb_path_query('{"a":1}', 'strict $.k');
-- regression
SELECT bi('\377')::macaddr;
SELECT bi('1ff:00:00:00:00:00\377')::macaddr;
SELECT pg_input_error_info(bi('\377'), 'macaddr');
-- regression
SELECT bi('1.2.3.\377')::inet;
SELECT bi('1.2.3.\377')::cidr;
SELECT bi('1.2.3.4/24\377')::cidr;
SELECT pg_input_error_info(bi('1.2.3.\377'), 'inet');

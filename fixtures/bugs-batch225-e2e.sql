-- Two-binary differential corpus for the 2026-09-13 bug-inventory batch
-- batch-225-backend-utils-adt (C 18.6 oracle vs pgrust). Each leg is one row
-- of the batch (utils/adt: raw input bytes echoed in typinput / parser
-- messages, identifier and zone-name clipping on character boundaries,
-- %m expansion never touching a filename, builtin-locale encoding names).
\set VERBOSITY verbose
CREATE DATABASE b225e2e TEMPLATE template0 ENCODING 'UTF8';
\c b225e2e
\set VERBOSITY verbose
-- byteain alias: raw (non-UTF-8) bytes into a text value.
CREATE FUNCTION b225_bi(cstring) RETURNS text AS 'byteain' LANGUAGE internal;
-- regression: to_ascii_encname echoes the name bytes (ascii.c:128).
SELECT to_ascii('abc', b225_bi('\xff')::name);
SELECT to_ascii('abc', 'nope');
-- regression: ArrayGetIntegerTypmods quotes the typmod bytes (numutils.c:617).
SELECT intervaltypmodin(ARRAY[b225_bi('\xff')]::cstring[]);
SELECT intervaltypmodin(ARRAY['1' || b225_bi('\xff')]::cstring[]);
-- regression: pg_lsn_in (pg_lsn.c:73).
SELECT b225_bi('\xff')::pg_lsn;
SELECT pg_input_is_valid(b225_bi('\xff'), 'pg_lsn');
-- regression: boolin (bool.c:148), hard and soft paths.
SELECT b225_bi('\xff')::bool;
SELECT pg_input_is_valid(b225_bi('\xff'), 'bool');
SELECT length(message), md5(message) FROM pg_input_error_info('x' || b225_bi('\xff') || 'y', 'bool');
-- regression: cash_in (cash.c:368).
SELECT b225_bi('\xff')::money;
SELECT ('1' || b225_bi('\xff'))::money;
-- regression / regression: downcase_truncate_identifier clips
-- the unit name at NAMEDATALEN-1 on a character boundary (date.c:1145, 3001).
SELECT date_part(repeat('é', 40), current_date);
SELECT date_part(repeat('é', 40), '12:00+00'::timetz);
SELECT date_part(repeat('é', 40), '12:00'::time);
SELECT extract(xyz from current_date);
SELECT date_part(b225_bi('\xff'), '12:00'::time);
-- regression: text_to_cstring_buffer keeps whole characters in the
-- 255-byte zone name (date.c:3139).
SELECT timezone(repeat('é', 200), '12:00+00'::timetz);
SELECT timezone(repeat('é', 200), '2020-01-01 12:00+00'::timestamptz);
-- regression: pg_base64_decode quotes the offending mbchar (encode.c:377).
SELECT decode(b225_bi('\xff'), 'base64');
SELECT decode(b225_bi('\xc3a9'), 'base64');
-- regression: %m expands in the format only (genfile.c:123).
SELECT pg_read_file('nope%m');
SELECT pg_read_binary_file('nope%m');
SELECT pg_stat_file('nope%m');
SELECT pg_ls_dir('nope%m');
-- regression: int2in/int4in/int2vectorin (int.c:198).
SELECT b225_bi('\xff')::int2;
SELECT b225_bi('\xff')::int4;
SELECT b225_bi('\xff')::int2vector;
SELECT ('1 ' || b225_bi('\xff'))::int2vector;
SELECT ('99999 ' || b225_bi('\xff'))::int2vector;
SELECT ('99999' || b225_bi('\xff'))::int2;
SELECT length(message), md5(message) FROM pg_input_error_info('x' || b225_bi('\xff'), 'int4');
-- regression: int8in (int8.c:54).
SELECT b225_bi('\xff')::int8;
SELECT ('1' || b225_bi('\xff'))::int8;
SELECT ('99999999999999999999' || b225_bi('\xff'))::int8;
-- regression: duplicate key rendered verbatim (json.c:1126, 1290).
SELECT json_object_agg_unique(k, 1) FROM (VALUES (b225_bi('\xff')), (b225_bi('\xff'))) v(k);
SELECT json_object(b225_bi('\xff') VALUE 1, b225_bi('\xff') VALUE 2 WITH UNIQUE KEYS);
-- regression / regression: setPathArray (jsonfuncs.c:5445).
SELECT jsonb_set('[1]', ARRAY[b225_bi('\xff')], '2');
CREATE TABLE b225_jt(j jsonb);
INSERT INTO b225_jt VALUES ('[1]');
UPDATE b225_jt SET j[b225_bi('\xff')] = '2';
-- regression: json_errdetail / report_json_context copy the input
-- excerpt verbatim (jsonfuncs.c:677).
SELECT b225_bi('\xff')::json;
SELECT ('[1,' || b225_bi('\xff') || ']')::json;
SELECT ('"' || b225_bi('\xff'))::json;
SELECT ('{"a":1, "b":' || repeat('x', 60) || b225_bi('\xff'))::json;
SELECT * FROM pg_input_error_info('[1,' || b225_bi('\xff') || ']', 'json');
SELECT length(detail), md5(detail) FROM pg_input_error_info('[1,' || b225_bi('\xff') || ']', 'json');
-- regression: parse_jsonb_index_flags (jsonfuncs.c:5642).
SELECT jsonb_to_tsvector('"a"'::jsonb, jsonb_build_array(b225_bi('\xff')));
SELECT jsonb_to_tsvector('"a"'::jsonb, '["nope"]');
-- regression: jsonpath_yyerror quotes yytext (jsonpath_scan.l:385).
SELECT ('$ 1' || b225_bi('\xff'))::jsonpath;
SELECT ('$.' || b225_bi('\xff') || ' 1')::jsonpath;
SELECT b225_bi('\xff')::jsonpath;
-- regression: builtin_validate_locale names the encoding
-- (pg_locale.c:1598).
CREATE DATABASE b225x LOCALE_PROVIDER builtin BUILTIN_LOCALE 'C.UTF-8' ENCODING 'SQL_ASCII' TEMPLATE template0;
CREATE COLLATION b225c (provider = builtin, locale = 'C.UTF-8');
DROP COLLATION b225c;

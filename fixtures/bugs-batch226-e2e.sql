-- Two-binary differential corpus for the 2026-09-13 bug-inventory batch
-- batch-226-backend-utils-adt (C 18.6 oracle vs pgrust). Each leg is one row
-- of the batch (utils/adt: raw input bytes echoed in parser messages,
-- record_recv's FORMAT_TYPE_ALLOW_INVALID names, a phrase operator ending
-- the tsquery, format() specifier bytes under SQL_ASCII, the temp-namespace
-- alias under quote_all_identifiers, cache_locale_time's DEBUG3 line, libxml
-- warnings ahead of the parse error).
\set VERBOSITY verbose
CREATE DATABASE b226e2e TEMPLATE template0 ENCODING 'UTF8';
\c b226e2e
\set VERBOSITY verbose
-- byteain alias: raw (non-UTF-8) bytes into a text value.
CREATE FUNCTION b226_bi(cstring) RETURNS text AS 'byteain' LANGUAGE internal;
-- regression: cache_locale_time() logs its locale at DEBUG3
-- (pg_locale.c:744); SET lc_time invalidates the cache each time.
SET client_min_messages = debug3;
SET lc_time = 'C';
SELECT to_char('2020-02-01'::date, 'TMMonth TMDay');
SELECT to_char('2020-02-01'::date, 'TMMon TMDy');
SET lc_time = 'C';
SELECT to_char('2020-02-01'::date, 'TMMonth');
RESET client_min_messages;
RESET lc_time;
-- regression: record_recv names a mismatched column type with
-- FORMAT_TYPE_ALLOW_INVALID ('-' for OID 0, '???' for an unknown OID)
-- (rowtypes.c:601); one composite column, binary COPY built by printf.
CREATE TYPE b226_r AS (a int);
CREATE TABLE b226_tr(x b226_r);
COPY b226_tr FROM PROGRAM 'printf ''PGCOPY\n\377\r\n\000\000\000\000\000\000\000\000\000\000\001\000\000\000\020\000\000\000\001\000\000\000\000\000\000\000\004\000\000\000\001\377\377''' WITH (FORMAT binary);
COPY b226_tr FROM PROGRAM 'printf ''PGCOPY\n\377\r\n\000\000\000\000\000\000\000\000\000\000\001\000\000\000\020\000\000\000\001\000\000\047\017\000\000\000\004\000\000\000\001\377\377''' WITH (FORMAT binary);
COPY b226_tr FROM PROGRAM 'printf ''PGCOPY\n\377\r\n\000\000\000\000\000\000\000\000\000\000\001\000\000\000\020\000\000\000\001\000\000\000\027\000\000\000\004\000\000\000\001\377\377''' WITH (FORMAT binary);
SELECT * FROM b226_tr;
-- regression: parse_phrase_operator stops at the terminator before
-- PHRASE_FINISH (tsquery.c:178): an operator ending the input is a syntax
-- error; one followed by whitespace is a missing operand.
SELECT 'a <->'::tsquery;
SELECT 'a <2>'::tsquery;
SELECT 'a <'::tsquery;
SELECT 'a <-'::tsquery;
SELECT 'a <2'::tsquery;
SELECT 'a <-> '::tsquery;
SELECT 'a <-> b'::tsquery;
SELECT pg_input_is_valid('a <->', 'tsquery');
SELECT * FROM pg_input_error_info('a <->', 'tsquery');
-- regression / regression: the parsers echo the input bytes
-- verbatim (tsquery.c:338, tsvector_parser.c:148/220/334).
SELECT (b226_bi('\xff') || ' &')::tsquery;
SELECT (b226_bi('\xff') || ' & )')::tsquery;
SELECT ('''' || b226_bi('\xff'))::tsquery;
SELECT (b226_bi('\xff') || ' \')::tsvector;
SELECT (b226_bi('\xff') || ':0')::tsvector;
SELECT ('''' || b226_bi('\xff'))::tsvector;
SELECT * FROM pg_input_error_info(b226_bi('\xff') || ' &', 'tsquery');
SELECT * FROM pg_input_error_info(b226_bi('\xff') || ':0', 'tsvector');
SELECT to_tsquery('simple', b226_bi('\xff') || ' &');
-- regression: parse_snapshot echoes the input bytes (xid8funcs.c:323).
SELECT b226_bi('\xff')::pg_snapshot;
SELECT ('10:20:' || b226_bi('\xff'))::pg_snapshot;
SELECT pg_input_is_valid(b226_bi('\xff'), 'pg_snapshot');
SELECT * FROM pg_input_error_info('10:20:' || b226_bi('\xff'), 'pg_snapshot');
-- regression: pg_split_walfile_name echoes the name bytes
-- (xlogfuncs.c:487).
SELECT pg_split_walfile_name(b226_bi('\xff'));
SELECT pg_split_walfile_name(b226_bi('00000001000000000000000\377'));
-- regression: format_type_be_qualified's pg_temp alias
-- goes through quote_qualified_identifier (objectaddress.c:4916).
CREATE TEMP TABLE b226_tt(a int);
SELECT type, identity FROM pg_identify_object('pg_type'::regclass, 'b226_tt'::regtype, 0);
SET quote_all_identifiers = on;
SELECT type, identity FROM pg_identify_object('pg_type'::regclass, 'b226_tt'::regtype, 0);
SELECT type, identity FROM pg_identify_object('pg_type'::regclass, 'b226_r'::regtype, 0);
SELECT pg_describe_object('pg_type'::regclass, 'b226_tt'::regtype, 0);
RESET quote_all_identifiers;
-- regression: text_format quotes the specifier's pg_mblen_range
-- bytes (varlena.c:6024): one byte under SQL_ASCII, the whole character
-- under UTF8.
SELECT format('%é');
SELECT format('%' || b226_bi('\xff'));
CREATE DATABASE b226sa TEMPLATE template0 ENCODING 'SQL_ASCII' LOCALE 'C';
\c b226sa
\set VERBOSITY verbose
CREATE FUNCTION b226_bi(cstring) RETURNS text AS 'byteain' LANGUAGE internal;
SELECT format('%é');
SELECT format('%' || b226_bi('\xff') || 'x');
SELECT format('%s %' || b226_bi('\xc3a9') || 'z', 1);
SELECT (b226_bi('\xff') || ' &')::tsquery;
SELECT b226_bi('\xff')::pg_snapshot;
\c b226e2e
\set VERBOSITY verbose
-- regression: libxml warnings are reported as they arrive (xml.c:2253),
-- so they precede the parse error. Expected lines hand-verified against
-- xml.c (the campaign's C oracle is built --without-libxml).
SELECT xpath('/*', '<relativens xmlns=''relative''/>');
SELECT xmlparse(document '<?xml version="1.5"?><a/>');
SELECT xmlparse(document '<?xml version="1.5"?><a/><b/>');

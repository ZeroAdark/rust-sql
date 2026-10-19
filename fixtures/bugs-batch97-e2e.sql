-- Differential fixture for the 2026-09-13 audit bug batch 97
-- (contrib/hstore). Run via scripts/bugs-batch97-e2e.sh against the frozen
-- expected captured from C PostgreSQL 18.6.
\pset pager off
\set VERBOSITY verbose
CREATE EXTENSION hstore;
-- regression / #4 / bug_e5480d1f: type_is_rowtype accepts
-- a domain over composite and lookup_rowtype_tupdesc_domain peels it, so
-- populate_record / hstore(record) take a domain first argument (and apply
-- the domain's constraints).
CREATE TYPE b97_r AS (a integer, b text);
CREATE DOMAIN b97_d AS b97_r;
CREATE DOMAIN b97_dchk AS b97_r CHECK ((VALUE).a > 0);
SELECT populate_record(NULL::b97_d, 'a=>1'::hstore);
SELECT populate_record(NULL::b97_d, 'a=>1, b=>x'::hstore);
SELECT populate_record(ROW(5,'q')::b97_d, 'a=>7'::hstore);
SELECT populate_record(ROW(5,'q')::b97_d, NULL::hstore);
SELECT populate_record(NULL::b97_dchk, 'a=>1'::hstore);
SELECT populate_record(NULL::b97_dchk, 'a=>-1'::hstore);
SELECT populate_record(ROW(5,'q')::b97_dchk, 'a=>-3'::hstore);
SELECT populate_record(NULL::b97_d, NULL::hstore);
SELECT populate_record(NULL::b97_d, ''::hstore);
SELECT populate_record(NULL::int, 'a=>1'::hstore);
SELECT hstore(NULL::b97_d);
SELECT hstore(ROW(3,'z')::b97_d);
SELECT hstore(ROW(3,'z')::b97_dchk);
SELECT hstore(NULL::b97_dchk);
SELECT NULL::b97_d #= 'a=>9'::hstore;
SELECT ROW(1,'y')::b97_d #= 'b=>w'::hstore;
SELECT pg_typeof(populate_record(NULL::b97_d, 'a=>1'::hstore));
-- regression / hstore_op#4: the HSTORE_POLLUTE legacy
-- symbol names (tconvert, fetchval, ...) resolve in the hstore library.
CREATE FUNCTION b97_tconvert(text,text) RETURNS hstore AS '$libdir/hstore', 'tconvert' LANGUAGE C IMMUTABLE;
CREATE FUNCTION b97_fetchval(hstore, text) RETURNS text AS '$libdir/hstore', 'fetchval' LANGUAGE C IMMUTABLE STRICT;
CREATE FUNCTION b97_exists(hstore, text) RETURNS bool AS '$libdir/hstore', 'exists' LANGUAGE C IMMUTABLE STRICT;
CREATE FUNCTION b97_defined(hstore, text) RETURNS bool AS '$libdir/hstore', 'defined' LANGUAGE C IMMUTABLE STRICT;
CREATE FUNCTION b97_delete(hstore, text) RETURNS hstore AS '$libdir/hstore', 'delete' LANGUAGE C IMMUTABLE STRICT;
CREATE FUNCTION b97_hs_concat(hstore, hstore) RETURNS hstore AS '$libdir/hstore', 'hs_concat' LANGUAGE C IMMUTABLE STRICT;
CREATE FUNCTION b97_hs_contains(hstore, hstore) RETURNS bool AS '$libdir/hstore', 'hs_contains' LANGUAGE C IMMUTABLE STRICT;
CREATE FUNCTION b97_hs_contained(hstore, hstore) RETURNS bool AS '$libdir/hstore', 'hs_contained' LANGUAGE C IMMUTABLE STRICT;
CREATE FUNCTION b97_akeys(hstore) RETURNS text[] AS '$libdir/hstore', 'akeys' LANGUAGE C IMMUTABLE STRICT;
CREATE FUNCTION b97_avals(hstore) RETURNS text[] AS '$libdir/hstore', 'avals' LANGUAGE C IMMUTABLE STRICT;
CREATE FUNCTION b97_skeys(hstore) RETURNS SETOF text AS '$libdir/hstore', 'skeys' LANGUAGE C IMMUTABLE STRICT;
CREATE FUNCTION b97_svals(hstore) RETURNS SETOF text AS '$libdir/hstore', 'svals' LANGUAGE C IMMUTABLE STRICT;
CREATE FUNCTION b97_each(IN hs hstore, OUT key text, OUT value text) RETURNS SETOF record AS '$libdir/hstore', 'each' LANGUAGE C IMMUTABLE STRICT;
SELECT b97_tconvert('a','b'), b97_tconvert('a',NULL), b97_tconvert(NULL,'b');
SELECT b97_fetchval('a=>b'::hstore, 'a'), b97_exists('a=>b'::hstore,'a'), b97_defined('a=>NULL'::hstore,'a');
SELECT b97_delete('a=>b,c=>d'::hstore,'a'), b97_hs_concat('a=>b'::hstore,'c=>d'::hstore);
SELECT b97_hs_contains('a=>b,c=>d'::hstore,'a=>b'::hstore), b97_hs_contained('a=>b'::hstore,'a=>b,c=>d'::hstore);
SELECT b97_akeys('a=>b,c=>d'::hstore), b97_avals('a=>b,c=>d'::hstore);
SELECT * FROM b97_skeys('a=>b,c=>d'::hstore);
SELECT * FROM b97_svals('a=>b,c=>NULL'::hstore);
SELECT * FROM b97_each('a=>b,c=>NULL'::hstore);
-- regression: skeys / svals / each return value-per-call,
-- so a target-list call needs no tuplestore (FROM-clause calls still
-- materialize through the FunctionScan, on both engines).
SET work_mem = '64kB';
SET temp_file_limit = 0;
SELECT length(svals(hstore('k', repeat('x', 1000000)))) LIMIT 1;
SELECT length(skeys(hstore(repeat('x', 1000000), 'k'))) LIMIT 1;
SELECT length((e).value) FROM (SELECT each(hstore('k', repeat('x', 1000000))) AS e) s;
SELECT length(v) FROM svals(hstore('k', repeat('x', 1000000))) v;
RESET temp_file_limit;
RESET work_mem;
SELECT skeys('a=>1, b=>NULL, cc=>3'::hstore);
SELECT svals('a=>1, b=>NULL, cc=>3'::hstore);
SELECT each('a=>1, b=>NULL, cc=>3'::hstore);
SELECT (each('a=>1, b=>NULL, cc=>3'::hstore)).*;
SELECT * FROM each('a=>1, b=>NULL, cc=>3'::hstore);
SELECT * FROM each(''::hstore);
SELECT * FROM svals(''::hstore);
SELECT * FROM skeys('a=>1'::hstore) WITH ORDINALITY;
SELECT * FROM ROWS FROM (skeys('a=>1, b=>2'::hstore), svals('x=>9'::hstore));
SELECT h, (each(h)).key, (each(h)).value FROM (VALUES ('a=>1, b=>2'::hstore), ('c=>NULL'::hstore)) v(h);
SELECT count(*) FROM (SELECT svals(h) FROM (VALUES ('a=>1, b=>2'::hstore), (''::hstore), ('c=>3'::hstore)) v(h)) s;
SELECT key, value FROM (VALUES ('z=>1'::hstore)) v(h), LATERAL each(h);
SELECT svals('a=>1, b=>NULL'::hstore) IS NULL;
-- bug_74b20fc4: the syntax-error snippet is the pg_mblen_cstr bytes at the
-- error position in the server encoding, sent verbatim; in a SQL_ASCII
-- database that is exactly one byte, not a UTF-8 character.
CREATE DATABASE b97ascii TEMPLATE template0 ENCODING 'SQL_ASCII';
\c b97ascii
\set VERBOSITY verbose
CREATE EXTENSION hstore;
SELECT ('a=>b ' || convert_from('\xE9'::bytea, 'SQL_ASCII'))::hstore;
SELECT ('a=>b ' || convert_from('\xE9AA'::bytea, 'SQL_ASCII'))::hstore;
SELECT ('a=>b ' || convert_from('\xC3A9'::bytea, 'SQL_ASCII'))::hstore;
SELECT ('a=' || convert_from('\xE9'::bytea, 'SQL_ASCII'))::hstore;
SELECT 'a=>b x'::hstore;
SELECT 'a=b'::hstore;
SELECT 'a=>b, '::hstore;
\c postgres
\set VERBOSITY verbose
SELECT E'a=>b é'::hstore;
SELECT E'a=>b \U0001F600'::hstore;
SELECT 'a=>b x'::hstore;
DROP DATABASE b97ascii;

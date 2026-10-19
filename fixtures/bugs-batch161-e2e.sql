-- Differential fixture for the 2026-09-13 audit bug batch 161
-- (backend/utils/adt: internal-alias frames of int8inc_any/int2vectorsend,
-- json_typeof stray tokens, jsonpath EXISTS in expr position, pg_lock_status /
-- pg_get_keywords / pg_get_catalog_foreign_keys / pg_input_error_info through
-- RETURNS record aliases, pg_current_logfile arity, parse_ident NUL). Run via
-- scripts/bugs-batch161-e2e.sh against the frozen expected captured from C
-- PostgreSQL 18.6, except the hand-verified block at the end.
\set VERBOSITY verbose
\pset pager off

-- regression: int8inc_any & co forward to int8inc (arg0 only); a one-arg
-- alias frame must not trip an arity check (int8.c:806-820)
CREATE FUNCTION b161_inc1(bigint) RETURNS bigint LANGUAGE internal AS 'int8inc_any';
SELECT b161_inc1(41);
CREATE FUNCTION b161_dec1(bigint) RETURNS bigint LANGUAGE internal AS 'int8dec_any';
SELECT b161_dec1(41);
CREATE FUNCTION b161_incf1(bigint) RETURNS bigint LANGUAGE internal AS 'int8inc_float8_float8';
SELECT b161_incf1(41);

-- regression: int2vectorsend is array_send, element type from the header
CREATE FUNCTION b161_i2vs(integer[]) RETURNS bytea LANGUAGE internal AS 'int2vectorsend';
SELECT b161_i2vs('{1,2,3}'::int[]);
SELECT int2vectorsend('1 2 3'::int2vector);

-- regression: json_typeof on a stray token is elog(ERROR) (json.c:1909)
CREATE FUNCTION b161_tj(cstring) RETURNS json LANGUAGE internal AS 'textin';
SELECT json_typeof(b161_tj(']'));
SELECT json_typeof(b161_tj('}'));
SELECT json_typeof(b161_tj(''));
SELECT json_typeof(b161_tj('  "x"'));

-- regression: expr never starts with EXISTS_P; the keyword is
-- rejected before its operand is parsed (jsonpath_gram.y:224)
SELECT '$[exists(1e1000000)]'::jsonpath;
SELECT '$[exists(1)]'::jsonpath;
SELECT '$[exists (1)]'::jsonpath;
SELECT '$[1 to exists(1e1000000)]'::jsonpath;
SELECT '$ + exists(1e1000000)'::jsonpath;
SELECT '$ ? (1 * exists(1e1000000))'::jsonpath;
SELECT '$ == exists(1e1000000)'::jsonpath;
SELECT '$ ? (@ == exists (1))'::jsonpath;
SELECT 'exists(exists(1e1000000))'::jsonpath;
SELECT '-exists(1e1000000)'::jsonpath;
SELECT '$[1e1000000]'::jsonpath;
SELECT '$ ? (!exists(1))'::jsonpath;
SELECT '$ ? (exists(1) && exists($.a))'::jsonpath;

-- regression: pg_lock_status builds and blesses its own 16-column
-- descriptor (lockfuncs.c:115-149)
CREATE FUNCTION b161_ls() RETURNS SETOF record LANGUAGE internal AS 'pg_lock_status';
SELECT DISTINCT locktype, mode, granted FROM b161_ls() AS x(locktype text, database oid, relation oid, page int, tuple smallint, virtualxid text, transactionid xid, classid oid, objid oid, objsubid smallint, virtualtransaction text, pid int, mode text, granted bool, fastpath bool, waitstart timestamptz) WHERE locktype = 'virtualxid' AND pid = pg_backend_pid();
SELECT * FROM b161_ls() AS x(a int);
CREATE FUNCTION b161_ls1() RETURNS record LANGUAGE internal AS 'pg_lock_status';
SELECT b161_ls1();

-- regression: pg_get_keywords resolves RECORD through expectedDesc,
-- converts through the declared columns' input functions
-- (BuildTupleFromCStrings), and inits the SRF before resolving
CREATE FUNCTION b161_kw() RETURNS SETOF record LANGUAGE internal AS 'pg_get_keywords';
SELECT * FROM b161_kw() AS k(word text, catcode "char", barelabel bool, catdesc text, baredesc text) WHERE word = 'select';
SELECT * FROM b161_kw() AS k(word text, catcode "char", barelabel text, catdesc text, baredesc text) WHERE word = 'select';
SELECT * FROM b161_kw() AS k(word text, catcode text, barelabel integer, catdesc text, baredesc text) WHERE word = 'select';
SELECT * FROM b161_kw() AS k(word text, catcode "char", barelabel bool) WHERE word = 'select';
SELECT * FROM b161_kw() AS k(word name) WHERE word = 'select';
SELECT b161_kw();
CREATE FUNCTION b161_kw1() RETURNS record LANGUAGE internal AS 'pg_get_keywords';
SELECT b161_kw1();
CREATE FUNCTION b161_fk() RETURNS SETOF record LANGUAGE internal AS 'pg_get_catalog_foreign_keys';
SELECT * FROM b161_fk() AS k(fktable regclass, fkcols text[], pktable regclass, pkcols text[], is_array bool, is_opt bool) ORDER BY 1, 2 LIMIT 2;
SELECT * FROM b161_fk() AS k(fktable regclass, fkcols text[], pktable regclass) ORDER BY 1, 2 LIMIT 1;
SELECT b161_fk();
CREATE FUNCTION b161_fk1() RETURNS record LANGUAGE internal AS 'pg_get_catalog_foreign_keys';
SELECT b161_fk1();
CREATE FUNCTION b161_iei(text, text) RETURNS record LANGUAGE internal AS 'pg_input_error_info';
SELECT * FROM b161_iei('abc', 'int4') AS (message text, detail text, hint text, sql_error_code text);
SELECT * FROM b161_iei('42', 'int4') AS (message text, detail text, hint text, sql_error_code text);

-- regression: the primary pg_current_logfile entry point validates a
-- format argument when the frame carries one (misc.c:1013)
CREATE FUNCTION b161_clf(text) RETURNS text LANGUAGE internal AS 'pg_current_logfile';
SELECT b161_clf('bogus');
SELECT b161_clf(NULL) IS NULL OR b161_clf(NULL) <> '';

-- regression: parse_ident and the log format read the text through
-- text_to_cstring, so an embedded NUL ends the string (misc.c:869/1013)
CREATE FUNCTION b161_bi(cstring) RETURNS text LANGUAGE internal AS 'byteain';
SELECT parse_ident(b161_bi('abc\000def'), true);
SELECT parse_ident(b161_bi('abc\000def'), false);
SELECT parse_ident(b161_bi('abc.\000def'), true);
SELECT parse_ident(b161_bi('"abc\000def"'), true);
SELECT parse_ident(b161_bi('abc-\000'), true);
SELECT pg_current_logfile(b161_bi('stderr\000x')) IS NULL OR true;
SELECT pg_current_logfile(b161_bi('stderrx\000'));

DROP FUNCTION b161_inc1(bigint), b161_dec1(bigint), b161_incf1(bigint), b161_i2vs(integer[]), b161_tj(cstring), b161_ls1(), b161_kw1(), b161_fk1(), b161_iei(text, text), b161_clf(text), b161_bi(cstring);

-- Hand-verified block (pgrust deliberate divergence, NOT captured from C): a
-- column definition list wider than pg_get_keywords' five values, or not
-- shaped like pg_get_catalog_foreign_keys' six hardcoded datums, crashes the
-- C 18.6 backend (BuildTupleFromCStrings / heap_form_tuple read past values[]
-- or dereference an OID as a pointer: misc.c:485, misc.c:551); pgrust raises
-- the executor's tupledesc_match error instead. UPSTREAM-BUGS.md entry 20.
SELECT * FROM b161_kw() AS k(word text, catcode "char", barelabel bool, catdesc text, baredesc text, extra int) WHERE word = 'select';
SELECT * FROM b161_fk() AS k(fktable text, fkcols text[], pktable regclass, pkcols text[], is_array bool, is_opt bool) LIMIT 1;
SELECT * FROM b161_fk() AS k(fktable regclass, fkcols text[], pktable regclass, pkcols text[], is_array bool, is_opt bool, extra int) LIMIT 1;
DROP FUNCTION b161_kw(), b161_fk(), b161_ls();

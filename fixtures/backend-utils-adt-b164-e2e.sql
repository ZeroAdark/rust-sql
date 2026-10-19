-- Differential fixture for the 2026-09-13 bug-inventory batch
-- batch-164-backend-utils-adt (C 18.6 oracle vs pgrust): bpchar/varchar
-- no-change paths return the detoasted image, bit_in's truncated multibyte
-- digit, tsvector_unnest / ts_stat SRF context and row-type checks, pg_nextoid
-- with a non-UTF-8 column name. Run via scripts/backend-utils-adt-b164-e2e.sh
-- against the frozen expected captured from C PostgreSQL 18.6.
\set VERBOSITY verbose
\pset pager off

-- regression: bpchar()/varchar() return the detoasted source on their
-- no-change paths (varchar.c:286/297, :628/:630), so pg_column_size sees the
-- uncompressed image, not the inline-compressed column datum.
CREATE TABLE b164_bp (x char(5000));
INSERT INTO b164_bp VALUES (repeat('a', 4000));
SELECT pg_column_size(x) < 200 AS stored_compressed,
       pg_column_size(bpchar(x, 5004, false)) AS same_typmod,
       pg_column_size(bpchar(x, -1, false)) AS no_typmod
  FROM b164_bp;
CREATE TABLE b164_vc (x varchar(5000));
INSERT INTO b164_vc VALUES (repeat('a', 4000));
SELECT pg_column_size(x) < 200 AS stored_compressed,
       pg_column_size("varchar"(x, 5004 + (random() * 0)::int, false)) AS wider_typmod,
       pg_column_size("varchar"(x, -1 + (random() * 0)::int, false)) AS no_typmod
  FROM b164_vc;

-- regression: a lone UTF-8 lead byte at the end of the input is
-- pg_mblen_cstr's incomplete character (22021), not a "not a valid digit"
-- 22P02 built from the truncated bytes.
SELECT bit_in(bpcharout(bpchar((-23)::"char")), 0, -1);
SELECT bit_in(bpcharout(('1'::bpchar || bpchar((-23)::"char"))::bpchar), 0, -1);
SELECT varbit_in(bpcharout(('x1'::bpchar || bpchar((-23)::"char"))::bpchar), 0, -1);
SELECT bit_in(textout('1é'), 0, -1);
SELECT bit_in(textout('xg'), 0, -1);

-- regression / #3: an internal alias returning SETOF record needs a
-- column definition list ("return type must be a row type"), and a scalar
-- alias fails SRF_FIRSTCALL_INIT before the ts_stat SQL runs.
CREATE FUNCTION b164_unnest(tsvector) RETURNS SETOF record LANGUAGE internal AS 'tsvector_unnest';
SELECT b164_unnest('a:1 b:2'::tsvector);
SELECT * FROM b164_unnest('a:1 b:2'::tsvector) AS (l text, p smallint[], w text[]);
SELECT * FROM unnest('a:1B b:2'::tsvector);
CREATE FUNCTION b164_stat(text) RETURNS SETOF record LANGUAGE internal AS 'ts_stat1';
SELECT b164_stat('select ''a:1 b:2''::tsvector');
SELECT * FROM b164_stat('select ''a:1 b:2''::tsvector') AS (w text, d int, e int) ORDER BY 1;
SELECT b164_stat('bogus');
SELECT * FROM ts_stat('select ''a:1 b:2''::tsvector') ORDER BY 1;
CREATE FUNCTION b164_stat_scalar(text) RETURNS record LANGUAGE internal AS 'ts_stat1';
SELECT b164_stat_scalar('bogus');
SELECT b164_stat_scalar('select ''a:1''::tsvector');
CREATE FUNCTION b164_unnest_scalar(tsvector) RETURNS record LANGUAGE internal AS 'tsvector_unnest';
SELECT b164_unnest_scalar('a:1'::tsvector);

-- regression: pg_nextoid checks superuser first (catalog.c:659) and
-- reports a non-UTF-8 column name as an undefined column with the raw bytes
-- (catalog.c:683), never an internal "invalid UTF-8" error.
CREATE DATABASE b164a TEMPLATE template0 ENCODING 'SQL_ASCII' LOCALE 'C';
\c b164a
\set VERBOSITY verbose
CREATE ROLE b164_notemp LOGIN;
SELECT pg_nextoid('pg_class', 'nosuch', 'pg_class_oid_index');
SELECT pg_nextoid('pg_class', chr(255)::name, 'pg_class_oid_index');
SELECT pg_nextoid('pg_class', ('x' || chr(255))::name, 'pg_class_oid_index');
SELECT pg_nextoid('pg_class', 'oid', 'pg_class_oid_index') > 0 AS got_oid;
SET ROLE b164_notemp;
SELECT pg_nextoid('pg_class', chr(255)::name, 'pg_class_oid_index');
SELECT pg_nextoid('pg_class', 'oid', 'pg_class_oid_index');
RESET ROLE;
\c postgres
DROP DATABASE b164a;
DROP ROLE b164_notemp;

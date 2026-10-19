-- Differential fixture for the 2026-09-13 audit bug batch 167
-- (backend/utils/adt). Run via scripts/bugs-batch167-e2e.sh against the
-- frozen expected captured from C PostgreSQL 18.6 (the xpath statement's
-- expected is hand-verified from xml.c:4449: the oracle build lacks libxml).
\pset pager off
\set VERBOSITY verbose
-- regression: generate_operator_clause / add_cast_to quote the
-- operator and type namespaces with quote_identifier, so the RI query text
-- in the error CONTEXT reads OPERATOR(pg_catalog.=) and ::pg_catalog.text.
CREATE TABLE b167_pt(k text PRIMARY KEY);
CREATE TABLE b167_ft(k varchar(10) NOT NULL REFERENCES b167_pt ON DELETE SET NULL);
INSERT INTO b167_pt VALUES ('a');
INSERT INTO b167_ft VALUES ('a');
DELETE FROM b167_pt;
-- regression: regclassout decides visibility with RelationIsVisible,
-- which never reports the unqualified miss (no DEBUG1 line at debug1).
CREATE SCHEMA b167_s;
CREATE TABLE b167_s.tt(x int);
SET client_min_messages = debug1;
SELECT 'b167_s.tt'::regclass::text;
RESET client_min_messages;
-- The rows below quote input bytes verbatim in their messages: a SQL_ASCII
-- database carries a high byte (chr(255)) that is not UTF-8.
CREATE DATABASE b167ascii TEMPLATE template0 ENCODING 'SQL_ASCII';
\c b167ascii
\set VERBOSITY verbose
-- regression
SELECT regexp_replace('a', 'a', 'b', 'g' || chr(255) || 'i');
-- regression
CREATE TYPE b167_ct AS (a int, b text);
SELECT (chr(255))::b167_ct;
SELECT message, sql_error_code FROM pg_input_error_info('x' || chr(255), 'b167_ct');
-- regression
SELECT ('(' || chr(255))::tid;
SELECT message, sql_error_code FROM pg_input_error_info('(' || chr(255), 'tid');
-- regression
SELECT date_trunc(chr(255), '2020-01-01'::timestamp);
SELECT date_trunc('millennia' || chr(255), '2020-01-01'::timestamptz);
SELECT date_trunc(chr(255), '1 day'::interval);
SELECT date_part(chr(255), '2020-01-01'::timestamp);
-- regression
SELECT (chr(255))::uuid;
SELECT message, sql_error_code FROM pg_input_error_info(chr(255), 'uuid');
-- regression (expected hand-verified: xml.c:4449 prints the bytes)
SELECT xpath('/a', '<a/>', ARRAY[ARRAY['', 'u' || chr(255)]]);
-- regression: a 63-byte identifier clip can end mid-character
SELECT set_config('temp_tablespaces', repeat('a', 62) || convert_from('\xC3A9'::bytea, 'SQL_ASCII'), false);
-- regression
SELECT set_config('createrole_self_grant', repeat('a', 62) || convert_from('\xC3A9'::bytea, 'SQL_ASCII'), false);
-- regression
CREATE EXTENSION postgres_fdw;
CREATE SERVER b167_srv FOREIGN DATA WRAPPER postgres_fdw OPTIONS (extensions 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaé');
ALTER SERVER b167_srv OPTIONS (SET extensions 'zzé');
DROP SERVER b167_srv;
\c postgres
DROP DATABASE b167ascii;

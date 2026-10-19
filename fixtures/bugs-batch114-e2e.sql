-- Differential fixture for the 2026-09-13 audit bug batch 114
-- (access/common: a SQL-language send function on a binary-format FETCH).
-- Run via scripts/bugs-batch114-e2e.sh. The expected is HAND-VERIFIED, not
-- captured from C: PostgreSQL 18.6 segfaults on the STABLE/IMMUTABLE FETCH
-- (fmgr_sql -> GetActiveSnapshot with no active snapshot while printtup
-- drains the FETCH's tuplestore, see UPSTREAM-BUGS.md); the send function's
-- bytes are what SendFunctionCall must emit.
\pset pager off
CREATE TYPE b114_t;
CREATE FUNCTION b114_in(cstring) RETURNS b114_t AS 'textin' LANGUAGE internal IMMUTABLE STRICT;
CREATE FUNCTION b114_out(b114_t) RETURNS cstring AS 'textout' LANGUAGE internal IMMUTABLE STRICT;
CREATE TYPE b114_t (INPUT = b114_in, OUTPUT = b114_out, INTERNALLENGTH = variable);
CREATE FUNCTION b114_send(b114_t) RETURNS bytea LANGUAGE sql STABLE STRICT AS 'SELECT ''ab''::bytea';
ALTER TYPE b114_t SET (SEND = b114_send);
BEGIN;
DECLARE b114_c BINARY CURSOR FOR SELECT 'x'::b114_t;
FETCH b114_c;
COMMIT;
CREATE OR REPLACE FUNCTION b114_send(b114_t) RETURNS bytea LANGUAGE sql IMMUTABLE STRICT AS 'SELECT ''cd''::bytea';
BEGIN;
DECLARE b114_c BINARY CURSOR FOR SELECT 'x'::b114_t;
FETCH b114_c;
COMMIT;
CREATE OR REPLACE FUNCTION b114_send(b114_t) RETURNS bytea LANGUAGE sql VOLATILE STRICT AS 'SELECT ''ef''::bytea';
BEGIN;
DECLARE b114_c BINARY CURSOR FOR SELECT 'x'::b114_t;
FETCH b114_c;
COMMIT;
DROP TYPE b114_t CASCADE;

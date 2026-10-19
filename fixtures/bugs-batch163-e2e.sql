-- Two-binary differential corpus for the 2026-09-13 bug-inventory batch
-- batch-163-backend-utils-adt (C 18.6 oracle vs pgrust). Each leg is one row
-- of the batch (utils/adt: regproc lookups vs the pending temp namespace,
-- generate_series_timestamp through a scalar alias, pg_get_expr wrapping of
-- sublink target lists, RI compare NULL guards and column-level bulk check,
-- tsqueryrecv over a short payload, ts_rewrite over non-UTF-8 SQL text).
\set VERBOSITY verbose
CREATE DATABASE b163e2e TEMPLATE template0 ENCODING 'UTF8';
\c b163e2e
\set VERBOSITY verbose
-- regression: regprocin / format_procedure / format_operator walk the
-- recomputed path (namespace.c:1221) and never create the temp namespace.
CREATE ROLE b163_notemp LOGIN;
REVOKE TEMP ON DATABASE b163e2e FROM PUBLIC;
GRANT CONNECT ON DATABASE b163e2e TO b163_notemp;
SET search_path = pg_temp, public;
SELECT 'now'::regproc;
SELECT pg_my_temp_schema() = 0 AS no_temp_after_regproc;
SELECT 'int4pl(int4,int4)'::regprocedure, '+(int4,int4)'::regoperator;
SELECT pg_my_temp_schema() = 0 AS no_temp_after_format;
SET ROLE b163_notemp;
SELECT 'now'::regproc, 'int4pl(int4,int4)'::regprocedure::text;
SELECT to_regproc('nosuchfunc');
RESET ROLE;
RESET search_path;
-- regression: SRF_FIRSTCALL_INIT runs before the step / zone
-- checks (timestamp.c:6695, 6780), so a scalar alias fails with 0A000.
CREATE FUNCTION b163_gs(timestamp, timestamp, interval) RETURNS timestamp LANGUAGE internal AS 'generate_series_timestamp';
SELECT b163_gs('2020-01-01', '2020-01-02', '0');
CREATE FUNCTION b163_gstz(timestamptz, timestamptz, interval) RETURNS timestamptz LANGUAGE internal AS 'generate_series_timestamptz';
SELECT b163_gstz('2020-01-01', '2020-01-02', '0');
CREATE FUNCTION b163_gstz4(timestamptz, timestamptz, interval, text) RETURNS timestamptz LANGUAGE internal AS 'generate_series_timestamptz_at_zone';
SELECT b163_gstz4('2020-01-01', '2020-01-02', '1 day', 'Nowhere/Bogus');
SELECT count(*) FROM generate_series(timestamp '2020-01-01', '2020-01-03', '1 day');
-- regression: deparse_expression_pretty starts at
-- WRAP_COLUMN_DEFAULT (ruleutils.c:3689): sublink targets wrap per line.
CREATE TABLE b163_t(a int, b int);
CREATE TABLE b163_u(x int, y int);
CREATE POLICY p1 ON b163_t USING (EXISTS (SELECT x, y FROM b163_u WHERE x = a));
CREATE POLICY p2 ON b163_t USING ((a, b) IN (SELECT x, y FROM b163_u));
SELECT polname, pg_get_expr(polqual, polrelid) FROM pg_policy WHERE polrelid = 'b163_t'::regclass ORDER BY polname;
SELECT polname, pg_get_expr(polqual, polrelid, true) FROM pg_policy WHERE polrelid = 'b163_t'::regclass ORDER BY polname;
-- regression / p1#2: ri_CompareWithCast uses FunctionCall2Coll /
-- FunctionCall3 (ri_triggers.c:3079, 3107): a NULL from the equality operator
-- or the cast function is an XX000 error, never a silent no-check.
CREATE TYPE b163_mt;
CREATE FUNCTION b163_mt_in(cstring) RETURNS b163_mt LANGUAGE internal IMMUTABLE STRICT AS 'int4in';
CREATE FUNCTION b163_mt_out(b163_mt) RETURNS cstring LANGUAGE internal IMMUTABLE STRICT AS 'int4out';
CREATE TYPE b163_mt (INPUT = b163_mt_in, OUTPUT = b163_mt_out, LIKE = int4);
CREATE FUNCTION b163_mt_lt(b163_mt, b163_mt) RETURNS bool LANGUAGE internal IMMUTABLE STRICT AS 'int4lt';
CREATE FUNCTION b163_mt_le(b163_mt, b163_mt) RETURNS bool LANGUAGE internal IMMUTABLE STRICT AS 'int4le';
CREATE FUNCTION b163_mt_ge(b163_mt, b163_mt) RETURNS bool LANGUAGE internal IMMUTABLE STRICT AS 'int4ge';
CREATE FUNCTION b163_mt_gt(b163_mt, b163_mt) RETURNS bool LANGUAGE internal IMMUTABLE STRICT AS 'int4gt';
CREATE FUNCTION b163_mt_cmp(b163_mt, b163_mt) RETURNS int LANGUAGE internal IMMUTABLE STRICT AS 'btint4cmp';
CREATE FUNCTION b163_mt_eq_raw(b163_mt, b163_mt) RETURNS bool LANGUAGE internal IMMUTABLE STRICT AS 'int4eq';
CREATE FUNCTION b163_mt_eq(b163_mt, b163_mt) RETURNS bool LANGUAGE sql IMMUTABLE AS 'SELECT CASE WHEN current_setting(''b163.nulleq'', true) = ''on'' THEN NULL ELSE b163_mt_eq_raw($1, $2) END';
CREATE OPERATOR < (LEFTARG = b163_mt, RIGHTARG = b163_mt, FUNCTION = b163_mt_lt);
CREATE OPERATOR <= (LEFTARG = b163_mt, RIGHTARG = b163_mt, FUNCTION = b163_mt_le);
CREATE OPERATOR >= (LEFTARG = b163_mt, RIGHTARG = b163_mt, FUNCTION = b163_mt_ge);
CREATE OPERATOR > (LEFTARG = b163_mt, RIGHTARG = b163_mt, FUNCTION = b163_mt_gt);
CREATE OPERATOR = (LEFTARG = b163_mt, RIGHTARG = b163_mt, FUNCTION = b163_mt_eq, COMMUTATOR = =);
CREATE OPERATOR CLASS b163_mt_ops DEFAULT FOR TYPE b163_mt USING btree AS OPERATOR 1 <, OPERATOR 2 <=, OPERATOR 3 =, OPERATOR 4 >=, OPERATOR 5 >, FUNCTION 1 b163_mt_cmp(b163_mt, b163_mt);
CREATE TABLE b163_pk(k b163_mt PRIMARY KEY);
CREATE TABLE b163_fk(k b163_mt REFERENCES b163_pk);
INSERT INTO b163_pk VALUES ('1'), ('2');
INSERT INTO b163_fk VALUES ('1');
SET b163.nulleq = on;
DO $$
DECLARE ctx text;
BEGIN
  UPDATE b163_fk SET k = '1';
  RAISE NOTICE 'no error';
EXCEPTION WHEN OTHERS THEN
  GET STACKED DIAGNOSTICS ctx = PG_EXCEPTION_CONTEXT;
  RAISE NOTICE '% % (raised from the RI query: %)', SQLSTATE, regexp_replace(SQLERRM, '\d+', 'N'), position('FOR KEY SHARE' in ctx) > 0;
END $$;
RESET b163.nulleq;
CREATE TYPE b163_mt2;
CREATE FUNCTION b163_mt2_in(cstring) RETURNS b163_mt2 LANGUAGE internal IMMUTABLE STRICT AS 'int4in';
CREATE FUNCTION b163_mt2_out(b163_mt2) RETURNS cstring LANGUAGE internal IMMUTABLE STRICT AS 'int4out';
CREATE TYPE b163_mt2 (INPUT = b163_mt2_in, OUTPUT = b163_mt2_out, LIKE = int4);
CREATE FUNCTION b163_mt2_to_mt_raw(b163_mt2) RETURNS b163_mt LANGUAGE internal IMMUTABLE STRICT AS 'int4up';
CREATE FUNCTION b163_mt2_to_mt(b163_mt2) RETURNS b163_mt LANGUAGE sql IMMUTABLE AS 'SELECT CASE WHEN current_setting(''b163.nullcast'', true) = ''on'' THEN NULL ELSE b163_mt2_to_mt_raw($1) END';
CREATE CAST (b163_mt2 AS b163_mt) WITH FUNCTION b163_mt2_to_mt(b163_mt2) AS IMPLICIT;
CREATE TABLE b163_fk2(k b163_mt2 REFERENCES b163_pk);
INSERT INTO b163_fk2 VALUES ('1');
SET b163.nullcast = on;
DO $$
BEGIN
  UPDATE b163_fk2 SET k = '1';
  RAISE NOTICE 'no error';
EXCEPTION WHEN OTHERS THEN
  RAISE NOTICE '% %', SQLSTATE, regexp_replace(SQLERRM, '\d+', 'N');
END $$;
RESET b163.nullcast;
SELECT * FROM b163_fk2;
-- regression: RI_Initial_Check passes the key columns as
-- selectedCols (ri_triggers.c:1584), so column-level SELECT is enough for
-- the bulk validation query (no per-row FOR KEY SHARE index probes).
CREATE ROLE b163_colpriv LOGIN;
CREATE TABLE b163_pk3(k int PRIMARY KEY, extra int);
CREATE TABLE b163_fk3(k int);
INSERT INTO b163_pk3 VALUES (1,1),(2,2),(3,3);
INSERT INTO b163_fk3 VALUES (1),(2),(3),(1),(2);
GRANT REFERENCES (k), SELECT (k) ON b163_pk3 TO b163_colpriv;
ALTER TABLE b163_fk3 OWNER TO b163_colpriv;
SET ROLE b163_colpriv;
BEGIN;
ALTER TABLE b163_fk3 ADD CONSTRAINT b163_fk3_fk FOREIGN KEY (k) REFERENCES b163_pk3(k);
SELECT relname, seq_scan, idx_scan FROM pg_stat_xact_user_tables WHERE relname IN ('b163_pk3', 'b163_fk3') ORDER BY 1;
COMMIT;
RESET ROLE;
-- regression: tsqueryrecv sizes its item array by the declared count
-- (tsquery.c:1247); a short payload is a protocol error, not an allocation
-- failure.
CREATE TABLE b163_tsq(q tsquery);
COPY b163_tsq FROM PROGRAM 'printf ''PGCOPY\n\377\r\n\000\000\000\000\000\000\000\000\000\000\001\000\000\000\004\004\000\000\000\377\377''' WITH (FORMAT binary);
COPY b163_tsq FROM PROGRAM 'printf ''PGCOPY\n\377\r\n\000\000\000\000\000\000\000\000\000\000\001\000\000\000\011\000\000\000\001\001\000\000a\000\377\377''' WITH (FORMAT binary);
SELECT * FROM b163_tsq;
-- regression: an empty input query returns before the rewrite
-- SQL text is inspected (tsquery_rewrite.c:293).
CREATE DATABASE b163ascii TEMPLATE template0 ENCODING 'SQL_ASCII' LC_COLLATE 'C' LC_CTYPE 'C';
\c b163ascii
\set VERBOSITY verbose
SELECT ts_rewrite(''::tsquery, 'SELECT ''a''::tsquery, ''' || chr(233) || '''::tsquery');
SELECT ts_rewrite('a'::tsquery, 'SELECT ''a''::tsquery, ''b''::tsquery');

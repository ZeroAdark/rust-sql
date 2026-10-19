-- Two-binary differential corpus for the 2026-09-13 bug-inventory batch
-- batch-162-backend-utils-adt (C 18.6 oracle vs pgrust). Each leg is one row
-- of the batch (utils/adt: numeric, oid, oracle_compat, pgstatfuncs, regexp).
\set VERBOSITY verbose
CREATE DATABASE b162e2e TEMPLATE template0 ENCODING 'UTF8';
\c b162e2e
\set VERBOSITY verbose
-- regression: numeric_smaller/larger return the detoasted (4B-header) argument.
CREATE TABLE b162_nt(a numeric, b numeric);
INSERT INTO b162_nt VALUES (1, 2), (1.0, 1.00), (12345678901234567890, 3);
SELECT pg_column_size(a) sa, pg_column_size(b) sb, pg_column_size(numeric_smaller(a,b)) sm, pg_column_size(numeric_larger(a,b)) lg, numeric_smaller(a,b), numeric_larger(a,b), pg_column_size(least(a,b)) le FROM b162_nt;
-- regression: int8_avg / int2int4_sum read {count,sum} at ARR_DATA_PTR (ndim-dependent).
SELECT int8_avg(ARRAY[[0::bigint]]) avg2d, int2int4_sum(ARRAY[[0::bigint]]) sum2d, int8_avg(ARRAY[0::bigint, 0]) avg1d, int2int4_sum(ARRAY[2::bigint, 7]) sum1d;
SELECT int8_avg(ARRAY[[1::bigint]]) IS NULL, int2int4_sum(ARRAY[[1::bigint]]) IS NULL;
SELECT int2int4_sum(ARRAY[1::bigint]);
-- regression: oidvectorsend is array_send over the stored element type.
CREATE FUNCTION b162_ovs(bigint[]) RETURNS bytea LANGUAGE internal AS 'oidvectorsend';
SELECT b162_ovs(ARRAY[1::bigint, 2]), oidvectorsend('1 2'::oidvector);
-- regression: bytea trims / translate hand back the input datum on empty inputs.
CREATE TABLE b162_bt(b bytea, t text);
INSERT INTO b162_bt VALUES ('\x0102', 'ab'), ('', '');
SELECT pg_column_size(b) sb, pg_column_size(btrim(b, ''::bytea)) bt, pg_column_size(ltrim(b, ''::bytea)) lt, pg_column_size(rtrim(b, ''::bytea)) rt, pg_column_size(btrim(b, '\x01'::bytea)) bt1, btrim(b, '\x01'::bytea), pg_column_size(translate(t, '', 'x')) tr0, pg_column_size(translate(t, 'a', 'x')) tr1, translate(t, 'a', 'x'), pg_column_size(t) st FROM b162_bt;
-- regression: incompatible flags are rejected before the collation is resolved.
CREATE TABLE b162_ct(a text COLLATE "C", b text COLLATE "POSIX");
INSERT INTO b162_ct VALUES ('a', 'b');
SELECT regexp_matches(a||b, 'x', 'qx') FROM b162_ct;
SELECT regexp_like(a||b, 'x', 'qx') FROM b162_ct;
SELECT regexp_replace(a||b, 'x', 'y', 'qx') FROM b162_ct;
SELECT regexp_count(a||b, 'x', 1, 'qx') FROM b162_ct;
SELECT regexp_matches(a||b, 'x', 'q') FROM b162_ct;
SELECT regexp_like(a||b, 'x', 'q') FROM b162_ct;
SELECT regexp_matches('ab', 'x', 'xq');
SELECT regexp_like('ab', 'x', 'qx');
SELECT regexp_like('ab', 'b', 'q');
-- regression: similar_escape's allocation check covers the varlena header.
SELECT length(similar_escape(repeat('a', 357913934), NULL));
-- regression: pg_stat_get_progress_info snapshots backend status before validating the command.
BEGIN;
SET application_name = 'b162_before';
SAVEPOINT s;
SELECT count(*) FROM pg_stat_get_progress_info('bogus');
ROLLBACK TO s;
SET application_name = 'b162_after';
SELECT application_name FROM pg_stat_activity WHERE pid = pg_backend_pid();
COMMIT;
SELECT application_name FROM pg_stat_activity WHERE pid = pg_backend_pid();
-- regression: pg_stat_reset_slru accepts non-UTF-8 bytes (SQL_ASCII) and resets "other".
CREATE DATABASE b162a ENCODING 'SQL_ASCII' LC_COLLATE 'C' LC_CTYPE 'C' TEMPLATE template0;
\c b162a
\set VERBOSITY verbose
SELECT pg_stat_reset_slru(chr(255));
SELECT pg_stat_reset_slru('bogus');
SELECT name FROM pg_stat_slru WHERE stats_reset IS NOT NULL ORDER BY name;

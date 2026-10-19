-- bugs/batch-231-mixed-lowtail: C-vs-pgrust parity for the batch's fixes.
-- Expected file captured from C 18.6 (scripts/regress-diff.sh --capture).
\set VERBOSITY verbose

-- regression: bt_target_page_check's DEBUG2
-- per-page item count (verify_nbtree.c:1253).
CREATE EXTENSION amcheck;
CREATE TABLE b231_bt(a int);
INSERT INTO b231_bt SELECT generate_series(1, 10);
CREATE INDEX b231_bt_i ON b231_bt(a);
SET client_min_messages = debug2;
SELECT bt_index_check('b231_bt_i');
RESET client_min_messages;
DROP TABLE b231_bt;

-- regression / p2#4: the CONTEXT action keeps at most
-- 511 bytes (dblink_context_msg[512]).
CREATE EXTENSION dblink;
SELECT dblink_connect('b231c', 'host=' || current_setting('unix_socket_directories') || ' port=' || current_setting('port') || ' dbname=' || current_database() || ' user=postgres');
SELECT dblink_close('b231c', repeat('x', 600));
SELECT dblink_disconnect('b231c');

-- regression: hashm_version is printed signed.
CREATE EXTENSION pageinspect;
CREATE TABLE b231_h(a int);
CREATE INDEX b231_h_i ON b231_h USING hash (a);
SELECT hash_page_type(overlay(get_raw_page('b231_h_i', 0) placing '\xffffffff'::bytea from 29 for 4));
DROP TABLE b231_h;

-- regression: a ProjectSet SRF initializes its arguments before
-- its own EXECUTE check, so the argument's permission error comes first.
CREATE ROLE b231_r;
CREATE FUNCTION b231_srf(int) RETURNS SETOF int LANGUAGE sql AS 'select $1';
CREATE FUNCTION b231_sc() RETURNS int LANGUAGE sql AS 'select 1';
REVOKE EXECUTE ON FUNCTION b231_srf(int), b231_sc() FROM public;
SET ROLE b231_r;
SELECT b231_srf(b231_sc());
RESET ROLE;
GRANT EXECUTE ON FUNCTION b231_sc() TO b231_r;
SET ROLE b231_r;
SELECT b231_srf(b231_sc());
RESET ROLE;
DROP FUNCTION b231_srf(int), b231_sc();

-- regression: the routing root's own partition-constraint
-- failure reports the inserted columns even without SELECT privilege.
CREATE TABLE b231_proot(a int, b text) PARTITION BY RANGE (a);
CREATE TABLE b231_psub PARTITION OF b231_proot FOR VALUES FROM (0) TO (10) PARTITION BY RANGE (a);
CREATE TABLE b231_pleaf PARTITION OF b231_psub FOR VALUES FROM (0) TO (10);
GRANT INSERT ON b231_psub, b231_pleaf TO b231_r;
SET ROLE b231_r;
INSERT INTO b231_psub VALUES (99, 'x');
INSERT INTO b231_psub(a) VALUES (99);
INSERT INTO b231_psub VALUES (5, 'ok');
RESET ROLE;
SELECT * FROM b231_psub;
DROP TABLE b231_proot;
DROP ROLE b231_r;

-- regression: initialize_peragg checks strict-transfn /
-- NULL-initval input compatibility before the strictness mismatch. The
-- message carries the aggregate OID, so only its shape is compared.
CREATE FUNCTION b231_sf(text, int) RETURNS text LANGUAGE sql AS 'select coalesce($1, '''') || $2';
CREATE FUNCTION b231_inv(text, int) RETURNS text LANGUAGE sql AS 'select $1';
CREATE AGGREGATE b231_agg(int) (sfunc = b231_sf, stype = text, msfunc = b231_sf, minvfunc = b231_inv, mstype = text);
ALTER FUNCTION b231_sf(text, int) STRICT;
DO $$ BEGIN
  PERFORM b231_agg(a) OVER (ORDER BY a ROWS BETWEEN 1 PRECEDING AND CURRENT ROW) FROM (VALUES (1), (2)) v(a);
EXCEPTION WHEN OTHERS THEN
  RAISE NOTICE '% %', SQLSTATE, regexp_replace(SQLERRM, '[0-9]+', 'N');
END $$;
ALTER FUNCTION b231_inv(text, int) STRICT;
DO $$ BEGIN
  PERFORM b231_agg(a) OVER (ORDER BY a ROWS BETWEEN 1 PRECEDING AND CURRENT ROW) FROM (VALUES (1), (2)) v(a);
EXCEPTION WHEN OTHERS THEN
  RAISE NOTICE '% %', SQLSTATE, regexp_replace(SQLERRM, '[0-9]+', 'N');
END $$;
ALTER FUNCTION b231_sf(text, int) CALLED ON NULL INPUT;
SELECT b231_agg(a) OVER (ORDER BY a ROWS BETWEEN 1 PRECEDING AND CURRENT ROW) FROM (VALUES (1), (2)) v(a);
DROP AGGREGATE b231_agg(int);
DROP FUNCTION b231_sf(text, int), b231_inv(text, int);

-- regression: tuple_data_split measures the t_bits
-- bytes (strlen) and reports an invalid character's raw bytes.
CREATE TABLE b231_tds(a int, b int);
INSERT INTO b231_tds VALUES (1, NULL);
SELECT tuple_data_split('b231_tds'::regclass, t_data, t_infomask, t_infomask2, '1010x000') FROM heap_page_items(get_raw_page('b231_tds', 0));
SELECT tuple_data_split('b231_tds'::regclass, t_data, t_infomask, t_infomask2, '10é0000') FROM heap_page_items(get_raw_page('b231_tds', 0));
SELECT tuple_data_split('b231_tds'::regclass, t_data, t_infomask, t_infomask2, '10000000') FROM heap_page_items(get_raw_page('b231_tds', 0));
DROP TABLE b231_tds;

-- regression: an opclass without the hash support function
-- fails like index_getprocinfo (indexam.c:946).
CREATE EXTENSION bloom;
CREATE OPERATOR CLASS b231_oc FOR TYPE int4 USING bloom AS OPERATOR 1 =(int4, int4);
CREATE TABLE b231_bl(a int);
CREATE INDEX b231_bl_i ON b231_bl USING bloom (a b231_oc);
DROP TABLE b231_bl;
DROP OPERATOR CLASS b231_oc USING bloom;

-- SQL_ASCII arms: raw (non-UTF-8) bytes in messages, byte-for-byte as C.
-- regression, regression,
-- regression (t_bits length counts bytes).
CREATE DATABASE b231_sa ENCODING 'SQL_ASCII' TEMPLATE template0;
\c b231_sa
CREATE EXTENSION pg_logicalinspect;
CREATE EXTENSION pageinspect;
SELECT * FROM pg_get_logical_snapshot_meta(convert_from('\x41ff2d31'::bytea, 'SQL_ASCII'));
CREATE TABLE b231_pk(a text) PARTITION BY LIST (a);
CREATE TABLE b231_pk1 PARTITION OF b231_pk FOR VALUES IN ('a');
INSERT INTO b231_pk VALUES (convert_from('\x41ff42'::bytea, 'SQL_ASCII'));
INSERT INTO b231_pk VALUES (convert_from(('\x' || repeat('c3a9', 40))::bytea, 'SQL_ASCII'));
CREATE TABLE b231_tds(a int, b int);
INSERT INTO b231_tds VALUES (1, NULL);
SELECT tuple_data_split('b231_tds'::regclass, t_data, t_infomask, t_infomask2, convert_from('\x31ff'::bytea, 'SQL_ASCII')) FROM heap_page_items(get_raw_page('b231_tds', 0));
SELECT tuple_data_split('b231_tds'::regclass, t_data, t_infomask, t_infomask2, convert_from('\x3130ff3030303030'::bytea, 'SQL_ASCII')) FROM heap_page_items(get_raw_page('b231_tds', 0));
\c postgres
DROP DATABASE b231_sa;

-- bugs/batch-230-mixed-lowtail: pgrust-only witnesses (expected captured
-- from pgrust via scripts/regress-diff.sh --capture; the C install has no
-- test modules and pgrust's pool channel prints no worker-start line).
\set VERBOSITY verbose
-- regression: elog(DEBUG1, "EventTriggerInvoke %u").
CREATE FUNCTION b230_evf() RETURNS event_trigger LANGUAGE plpgsql AS $$ BEGIN END $$;
CREATE EVENT TRIGGER b230_evt ON ddl_command_start EXECUTE FUNCTION b230_evf();
SET client_min_messages = debug1;
CREATE TABLE b230_x(a int);
DROP TABLE b230_x;
RESET client_min_messages;
DROP EVENT TRIGGER b230_evt;
DROP FUNCTION b230_evf();
-- regression: the chunked btree bulkdelete announces
-- 'scanned index ...' at DEBUG2 like vac_bulkdel_one_index.
CREATE TABLE b230_pv(a int, b int);
INSERT INTO b230_pv SELECT g, g FROM generate_series(1,3000) g;
CREATE INDEX b230_pv_a ON b230_pv(a);
CREATE INDEX b230_pv_b ON b230_pv(b);
DELETE FROM b230_pv WHERE a % 3 = 0;
SET min_parallel_index_scan_size = 0;
SET client_min_messages = debug2;
VACUUM (PARALLEL 2) b230_pv;
RESET client_min_messages;
-- regression, regression: object-access
-- hooks (functioncmds.c:1526/2020, extension.c:3397/3685/3777).
LOAD 'test_oat_hooks';
SET test_oat_hooks.audit = on;
CREATE FUNCTION b230_hf(int) RETURNS int LANGUAGE sql AS 'SELECT $1';
ALTER FUNCTION b230_hf(int) IMMUTABLE;
CREATE TRANSFORM FOR int LANGUAGE plpgsql (FROM SQL WITH FUNCTION varchar_support(internal), TO SQL WITH FUNCTION int4recv(internal));
CREATE OR REPLACE TRANSFORM FOR int LANGUAGE plpgsql (FROM SQL WITH FUNCTION varchar_support(internal), TO SQL WITH FUNCTION int4recv(internal));
DROP TRANSFORM FOR int LANGUAGE plpgsql;
CREATE SCHEMA b230_s1;
CREATE SCHEMA b230_s2;
\getenv b230_ext B230_EXT_DIR
\set b230_ecp '$system:' :b230_ext
SET extension_control_path = :'b230_ecp';
CREATE EXTENSION b230ext SCHEMA b230_s1;
CREATE TABLE b230_s1.hx(a int);
ALTER EXTENSION b230ext ADD TABLE b230_s1.hx;
ALTER EXTENSION b230ext DROP TABLE b230_s1.hx;
ALTER EXTENSION b230ext SET SCHEMA b230_s2;
ALTER EXTENSION b230ext UPDATE TO '1.1';
SET test_oat_hooks.audit = off;
DROP EXTENSION b230ext;
DROP TABLE b230_s1.hx;
DROP FUNCTION b230_hf(int);
DROP SCHEMA b230_s1, b230_s2;

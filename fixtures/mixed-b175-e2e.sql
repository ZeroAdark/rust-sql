-- bugs/batch-175-mixed-xhigh: expected captured from C 18.6
-- (scripts/mixed-b175-e2e.sh).
\set VERBOSITY verbose
-- regression: a rescan that changes the pushed-down bound
-- destroys the sort; EXPLAIN ANALYZE shows no Sort Method for it.
SET enable_material = off;
SET enable_memoize = off;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) SELECT * FROM (VALUES (1),(0)) v(n), LATERAL (SELECT i FROM generate_series(1,5) i ORDER BY i DESC LIMIT v.n) s;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) SELECT * FROM (VALUES (0),(1)) v(n), LATERAL (SELECT i FROM generate_series(1,5) i ORDER BY i DESC LIMIT v.n) s;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF, FORMAT JSON) SELECT * FROM (VALUES (1),(0)) v(n), LATERAL (SELECT i FROM generate_series(1,5) i ORDER BY i DESC LIMIT v.n) s;
RESET enable_material;
RESET enable_memoize;
-- regression: pg_get_loaded_modules reports the magic-block
-- name (intarray), not the library file name (_int).
LOAD '_int';
LOAD 'cube';
SELECT module_name, file_name ~ '^_int\.' AS int_file FROM pg_get_loaded_modules() ORDER BY 1;
-- regression: ALTER EXTENSION SET SCHEMA keeps its pg_depend
-- scan (and locks) open while a member relocation waits.
CREATE EXTENSION dblink;
CREATE EXTENSION pg_stat_statements;
CREATE SCHEMA b175s;
SELECT dblink_connect('b175c', 'dbname=' || current_database() || ' port=' || current_setting('port') || ' host=' || current_setting('unix_socket_directories') || ' user=' || current_user);
BEGIN;
LOCK TABLE pg_stat_statements IN ACCESS EXCLUSIVE MODE;
SELECT dblink_send_query('b175c', 'ALTER EXTENSION pg_stat_statements SET SCHEMA b175s');
DO $$ BEGIN FOR i IN 1..200 LOOP EXIT WHEN EXISTS (SELECT 1 FROM pg_locks WHERE relation = 'pg_stat_statements'::regclass AND NOT granted); PERFORM pg_sleep(0.05); END LOOP; END $$;
SELECT relation::regclass AS rel, mode, granted FROM pg_locks WHERE pid <> pg_backend_pid() AND locktype = 'relation' AND relation IN ('pg_depend'::regclass, 'pg_depend_reference_index'::regclass, 'pg_extension'::regclass, 'pg_stat_statements'::regclass) ORDER BY 1::text, 2;
COMMIT;
SELECT * FROM dblink_get_result('b175c') AS t(status text);
SELECT dblink_disconnect('b175c');
SELECT extnamespace::regnamespace FROM pg_extension WHERE extname = 'pg_stat_statements';
ALTER EXTENSION pg_stat_statements SET SCHEMA public;
DROP EXTENSION pg_stat_statements;
DROP EXTENSION dblink;
DROP SCHEMA b175s;
-- regression: EXPLAIN ANALYZE reports the batch size the
-- INSERT ran with (ri_BatchSize), not the server option re-read afterwards.
CREATE EXTENSION postgres_fdw;
SELECT current_setting('port') AS b175_port, current_setting('unix_socket_directories') AS b175_sock, current_database() AS b175_db, current_user AS b175_user \gset
CREATE SERVER b175_srv FOREIGN DATA WRAPPER postgres_fdw OPTIONS (host :'b175_sock', port :'b175_port', dbname :'b175_db', batch_size '5');
CREATE USER MAPPING FOR CURRENT_USER SERVER b175_srv OPTIONS (user :'b175_user');
CREATE TABLE b175_remote(a int);
CREATE FOREIGN TABLE b175_ft(a int) SERVER b175_srv OPTIONS (table_name 'b175_remote');
CREATE FUNCTION b175_bump() RETURNS int LANGUAGE plpgsql VOLATILE AS $$ BEGIN EXECUTE 'ALTER SERVER b175_srv OPTIONS (SET batch_size ''17'')'; RETURN 1; END $$;
EXPLAIN (ANALYZE, VERBOSE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) INSERT INTO b175_ft SELECT b175_bump() FROM generate_series(1, 3);
EXPLAIN (VERBOSE, COSTS OFF) INSERT INTO b175_ft SELECT 1;
EXPLAIN (ANALYZE, VERBOSE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) INSERT INTO b175_ft SELECT 1;
SELECT count(*) FROM b175_remote;
DROP FOREIGN TABLE b175_ft;
DROP TABLE b175_remote;
DROP FUNCTION b175_bump();
DROP SERVER b175_srv CASCADE;
DROP EXTENSION postgres_fdw;

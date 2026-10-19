-- regression: statement_timeout bounds a statement parked at the
-- max_active_queries gate (pgrust-only setting; no C counterpart).
CREATE EXTENSION dblink;
CREATE ROLE gx LOGIN;
CREATE ROLE gy LOGIN;
ALTER SYSTEM SET max_active_queries = 1;
SELECT pg_reload_conf();
SELECT pg_sleep(1);
SHOW max_active_queries;
SELECT 'host=127.0.0.1 port=' || current_setting('port') || ' dbname=postgres' AS cs \gset
SELECT dblink_connect('x', :'cs' || ' user=gx application_name=gx');
SELECT dblink_connect('y', :'cs' || ' user=gy application_name=gy');
SELECT dblink_exec('x', 'SET statement_timeout = ''1s''');
SELECT dblink_exec('y', 'BEGIN');
SELECT * FROM dblink('y', 'SELECT 1') AS t(one int);
SELECT dblink_send_query('x', 'SELECT clock_timestamp()::text');
SELECT pg_sleep(4);
SELECT dblink_is_busy('x') AS x_busy_after_4s;
SELECT count(*) AS x_rows FROM dblink_get_result('x', false) AS t(r text);
SELECT dblink_error_message('x') AS x_error;
SELECT count(*) AS x_drained FROM dblink_get_result('x', false) AS t(r text);
SELECT dblink_exec('y', 'COMMIT');
SELECT * FROM dblink('x', 'SELECT 42') AS t(v int);
SELECT dblink_disconnect('x'), dblink_disconnect('y');
ALTER SYSTEM RESET max_active_queries;
SELECT pg_reload_conf();

-- Two-binary differential corpus for the 2026-09-13 bug-inventory batch
-- batch-207-interfaces-pgclient (C 18.6 oracle vs pgrust). The dblink
-- connections go back to the running server.
\set VERBOSITY verbose
CREATE DATABASE b207e2e TEMPLATE template0 ENCODING 'UTF8';
\c b207e2e
\set VERBOSITY verbose
CREATE EXTENSION dblink;
SELECT 'host=' || current_setting('unix_socket_directories') || ' port=' || current_setting('port') || ' dbname=' || current_database() || ' user=' || current_user AS c \gset
-- regression: a remote backend that dies mid-query
-- leaves libpq's whole error buffer behind — the FATAL, the three-line EOF
-- notice (fe-misc.c:833) and PQgetResult's "invalid socket" — and the next
-- send starts a clean cycle ("no connection to the server" alone).
SELECT dblink_connect('b207a', :'c');
SELECT dblink_exec('b207a', 'SELECT pg_terminate_backend(pg_backend_pid())');
SELECT dblink_error_message('b207a');
SELECT dblink_exec('b207a', 'SELECT 1');
SELECT dblink_error_message('b207a');
SELECT dblink_exec('b207a', 'SELECT 1', false);
SELECT dblink_disconnect('b207a');
SELECT dblink_connect('b207b', :'c');
SELECT * FROM dblink('b207b', 'SELECT pg_terminate_backend(pg_backend_pid())') AS t(x bool);
SELECT dblink_disconnect('b207b');
SELECT dblink_connect('b207c', :'c');
SELECT dblink_send_query('b207c', 'SELECT pg_terminate_backend(pg_backend_pid())');
SELECT * FROM dblink_get_result('b207c') AS t(x bool);
SELECT * FROM dblink_get_result('b207c') AS t(x bool);
SELECT dblink_error_message('b207c');
SELECT dblink_disconnect('b207c');

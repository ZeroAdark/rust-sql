-- Two-binary differential corpus for the 2026-09-13 bug-inventory batch
-- batch-101-interfaces-pgclient (C 18.6 oracle vs pgrust). Each leg is one
-- row of the batch; the connection goes back to the running server.
\set VERBOSITY verbose
CREATE DATABASE b101e2e TEMPLATE template0 ENCODING 'UTF8';
\c b101e2e
\set VERBOSITY verbose
CREATE EXTENSION dblink;
SELECT 'host=' || current_setting('unix_socket_directories') || ' port=' || current_setting('port') || ' dbname=' || current_database() || ' user=' || current_user AS c \gset
-- Connection failures name the socket directory, which differs per server:
-- report them with the path scrubbed.
CREATE FUNCTION b101_try(connstr text) RETURNS text LANGUAGE plpgsql AS $$
DECLARE s text; m text; d text;
BEGIN
  PERFORM dblink_connect('b101try', connstr);
  PERFORM dblink_disconnect('b101try');
  RETURN 'connected';
EXCEPTION WHEN OTHERS THEN
  GET STACKED DIAGNOSTICS s = RETURNED_SQLSTATE, m = MESSAGE_TEXT, d = PG_EXCEPTION_DETAIL;
  RETURN s || ' ' || m || E'\n' || regexp_replace(d, 'socket "[^"]*"', 'socket "SOCK"', 'g');
END $$;
-- regression: a zero-column SELECT is PGRES_TUPLES_OK and
-- meets the rowtype check (storeRow), never a command tag
SELECT * FROM dblink(:'c', 'SELECT WHERE false') AS t(x text);
SELECT * FROM dblink(:'c', 'SELECT WHERE true') AS t(x text);
SELECT dblink_connect('b101a', :'c');
SELECT dblink_exec('b101a', 'SELECT WHERE false');
SELECT dblink_exec('b101a', 'SELECT WHERE false', false);
SELECT dblink_send_query('b101a', 'SELECT WHERE false');
SELECT * FROM dblink_get_result('b101a') AS t(x text);
SELECT * FROM dblink_get_result('b101a') AS t(x text);
SELECT * FROM dblink('b101a', 'SELECT 1 WHERE false') AS t(x int);
SELECT dblink_disconnect('b101a');
-- regression: the connection is made with the conninfo's
-- own client_encoding; the database encoding is set only afterwards
SELECT b101_try(:'c' || ' client_encoding=not_an_encoding');
SELECT dblink_connect('b101b', :'c' || ' client_encoding=LATIN1');
SELECT * FROM dblink('b101b', 'SHOW client_encoding') AS t(x text);
SELECT dblink_disconnect('b101b');
-- regression: target_session_attrs is validated
-- and enforced before the connection is reported open
SELECT b101_try(:'c' || ' target_session_attrs=bogus');
SELECT b101_try(:'c' || ' target_session_attrs=');
SELECT b101_try(:'c' || ' target_session_attrs=any');
SELECT b101_try(:'c' || ' target_session_attrs=standby');
SELECT b101_try(:'c' || ' target_session_attrs=read-only');
SELECT b101_try(:'c' || ' target_session_attrs=primary');
SELECT b101_try(:'c' || ' target_session_attrs=read-write');
SELECT b101_try(:'c' || ' target_session_attrs=prefer-standby');
SELECT b101_try(:'c' || ' options=-cdefault_transaction_read_only=on target_session_attrs=read-write');
SELECT b101_try(:'c' || ' options=-cdefault_transaction_read_only=on target_session_attrs=read-only');
SELECT b101_try(:'c' || ' options=-cdefault_transaction_read_only=on target_session_attrs=primary');
-- connectFailureMessage (fe-connect.c:2681): the OS error and libpq's hint
SELECT b101_try('host=/nonexistent port=1 user=postgres');
SELECT b101_try('hostaddr=127.0.0.1 port=1 user=postgres');
SELECT b101_try('host=127.0.0.1 port=1 user=postgres');
DROP FUNCTION b101_try(text);

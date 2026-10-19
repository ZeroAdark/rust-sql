CREATE EXTENSION dblink;
\set SHOW_CONTEXT always

-- loopback over the server's own unix socket; engine-agnostic.
CREATE FUNCTION connection_parameters() RETURNS text LANGUAGE SQL AS $f$
       SELECT $$host='$$||current_setting('unix_socket_directories')||$$' user='$$||current_user||$$' dbname='$$||current_database()||$$' port=$$||current_setting('port');
$f$;

-- PQsendQueryStart: a connection with a command in flight refuses another
-- send; libpqsrv_exec has PQsendQuery's preconditions, so dblink_exec is
-- refused too rather than draining the pending result. The error buffer
-- accumulates until the next query cycle.
SELECT dblink_connect('busy', connection_parameters());
SELECT dblink_send_query('busy', 'SELECT 1');
SELECT dblink_send_query('busy', 'SELECT 2');
SELECT dblink_exec('busy', 'SELECT 3');
SELECT dblink_exec('busy', 'SELECT 3', false);
SELECT * FROM dblink('busy', 'SELECT 4', false) AS t(a int);
SELECT * FROM dblink_get_result('busy') AS t(a int);
SELECT * FROM dblink_get_result('busy') AS t(a int);
SELECT dblink_error_message('busy');

-- PQerrorMessage carries the syntax cursor display; a successful command
-- clears it (dblink_error_message reports OK).
SELECT dblink_exec('busy', 'SELECT nonsense', false);
SELECT dblink_error_message('busy');
SELECT dblink_exec('busy', 'CREATE TEMP TABLE busy_t(a int)');
SELECT dblink_error_message('busy');
SELECT dblink_exec('busy', 'SELCT
 1 +', false);
SELECT dblink_error_message('busy');
SELECT dblink_disconnect('busy');

-- startup packet: fallback_application_name applies only when no
-- application_name was given (an explicit empty one wins); an unquoted
-- value's terminal backslash is dropped by the conninfo scanner.
SELECT * FROM dblink(connection_parameters() || ' fallback_application_name=fb',
                     'SELECT application_name FROM pg_stat_activity WHERE pid = pg_backend_pid()') AS t(a text);
SELECT * FROM dblink(connection_parameters() || ' application_name='''' fallback_application_name=fb',
                     'SELECT application_name FROM pg_stat_activity WHERE pid = pg_backend_pid()') AS t(a text);
SELECT * FROM dblink(connection_parameters() || ' application_name=foo\',
                     'SELECT application_name FROM pg_stat_activity WHERE pid = pg_backend_pid()') AS t(a text);

-- min/max_protocol_version are validated before any socket is opened.
SELECT dblink_connect('bad', connection_parameters() || ' min_protocol_version=abc');
SELECT dblink_connect('bad', connection_parameters() || ' min_protocol_version=3.2 max_protocol_version=3.0');
SELECT dblink_connect('ok', connection_parameters() || ' min_protocol_version=3.0 max_protocol_version=latest');
SELECT dblink_disconnect('ok');

-- replication=database rides into the startup packet: a walsender answers.
SELECT dblink_connect('rep', connection_parameters() || ' replication=database');
SELECT dbname FROM dblink('rep', 'IDENTIFY_SYSTEM') AS t(systemid text, timeline int, xlogpos text, dbname text);
SELECT dblink_disconnect('rep');

-- PQnotifies parses input that arrived while the connection was idle: a
-- NOTIFY from another backend is seen by dblink_get_notify.
SELECT dblink_connect('lsn', connection_parameters());
SELECT dblink_exec('lsn', 'LISTEN busy_chan');
NOTIFY busy_chan, 'payload';
SELECT pg_sleep(1);
SELECT notify_name, extra FROM dblink_get_notify('lsn');
SELECT dblink_disconnect('lsn');

DROP FUNCTION connection_parameters();
DROP EXTENSION dblink;

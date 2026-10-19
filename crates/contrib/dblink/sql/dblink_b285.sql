CREATE EXTENSION dblink;
\set SHOW_CONTEXT always

-- loopback over the server's own unix socket; engine-agnostic.
CREATE FUNCTION connection_parameters() RETURNS text LANGUAGE SQL AS $f$
       SELECT $$host='$$||current_setting('unix_socket_directories')||$$' user='$$||current_user||$$' dbname='$$||current_database()||$$' port=$$||current_setting('port');
$f$;

-- An explicitly empty transport option is kept by the conninfo parser and
-- rejected by pqConnectOptions2 (fe-connect.c:1767): never a silent default.
SELECT dblink_connect('e', connection_parameters() || ' sslmode=');
SELECT dblink_connect('e', connection_parameters() || ' channel_binding=');
SELECT dblink_connect('e', connection_parameters() || ' sslnegotiation=');
SELECT dblink_connect('e', connection_parameters() || ' sslcertmode=');
SELECT dblink_connect('e', connection_parameters() || ' gssencmode=');
SELECT dblink_connect('e', connection_parameters() || ' sslmode='''' channel_binding=prefer');

-- TLS protocol bounds are validated on their text, sslmode=disable included.
SELECT dblink_connect('f', connection_parameters() || ' ssl_min_protocol_version=bogus');
SELECT dblink_connect('f', connection_parameters() || ' ssl_max_protocol_version=bogus');
SELECT dblink_connect('f', connection_parameters() || ' ssl_min_protocol_version=TLSv1.3 ssl_max_protocol_version=TLSv1.2');
SELECT dblink_connect('f', connection_parameters() || ' ssl_min_protocol_version=TLSv1.2 ssl_max_protocol_version=TLSv1');
SELECT dblink_connect('f', connection_parameters() || ' ssl_min_protocol_version= ssl_max_protocol_version=TLSv1');
SELECT dblink_connect('f', connection_parameters() || ' ssl_min_protocol_version=tlsv1.2 ssl_max_protocol_version=TLSV1.3 sslmode=disable');
SELECT dblink_disconnect('f');

-- load_balance_hosts: validated; random permutes a (here two-way identical)
-- host list and still connects.
SELECT dblink_connect('d', connection_parameters() || ' load_balance_hosts=bogus');
SELECT dblink_connect('d', connection_parameters() || ' load_balance_hosts=');
SELECT dblink_connect('d', connection_parameters() || ' load_balance_hosts=random');
SELECT dblink_disconnect('d');
SELECT dblink_connect('d', replace(connection_parameters(), 'host=''', 'host=''' || current_setting('unix_socket_directories') || ',') || ' load_balance_hosts=random');
SELECT dblink_disconnect('d');

-- A host starting with '@' is a Unix-domain (abstract) socket path, not a
-- host name to resolve (pqcomm.h is_unixsock_path, ip.c:215).
SELECT dblink_connect('a', 'host=@dblink-b285-abstract port=1 dbname=x user=y');

-- PQisBusy parses the buffered ErrorResponse (fe-protocol3.c:994): once
-- dblink_is_busy reports 0, dblink_error_message already carries the text.
SELECT dblink_connect('c', connection_parameters());
SELECT dblink_send_query('c', 'SELECT nonsense');
SELECT pg_sleep(0.5);
SELECT dblink_is_busy('c');
SELECT dblink_error_message('c');
SELECT dblink_is_busy('c');
SELECT dblink_error_message('c');
SELECT * FROM dblink_get_result('c') AS t(x int);
SELECT dblink_error_message('c');
SELECT * FROM dblink_get_result('c') AS t(x int);
SELECT dblink_exec('c', 'CREATE TEMP TABLE b285(a int)');
SELECT dblink_error_message('c');
SELECT dblink_disconnect('c');

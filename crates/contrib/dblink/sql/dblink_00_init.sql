CREATE EXTENSION dblink;
\set SHOW_CONTEXT always

-- Runs first in the module on a fresh cluster (no earlier dblink call on
-- either engine). dblink_init (dblink.c:268) registers the DblinkGetResult
-- wait event on the first call of ANY dblink function, before the
-- connection lookup fails; DblinkConnect only exists once dblink_connect
-- has run.
SELECT dblink_disconnect('no_such_connection');
SELECT type, name FROM pg_wait_events WHERE name LIKE 'Dblink%' ORDER BY name;

-- loopback over the server's own unix socket; engine-agnostic.
CREATE FUNCTION connection_parameters() RETURNS text LANGUAGE SQL AS $f$
       SELECT $$host='$$||current_setting('unix_socket_directories')||$$' user='$$||current_user||$$' dbname='$$||current_database()||$$' port=$$||current_setting('port');
$f$;

-- dblink_get_connections lists remoteConnHash in dynahash scan order
-- (NUMCONN = 16 initial buckets, string_hash chains, one bucket split per
-- insert past the load factor), not insertion or sorted order.
SELECT dblink_connect('alpha', connection_parameters());
SELECT dblink_connect('beta', connection_parameters());
SELECT dblink_connect('gamma', connection_parameters());
SELECT dblink_connect('delta', connection_parameters());
SELECT dblink_connect('dtest1', connection_parameters());
SELECT dblink_get_connections();
SELECT dblink_disconnect('beta');
SELECT dblink_connect('epsilon', connection_parameters());
SELECT dblink_get_connections();
SELECT dblink_disconnect(c) FROM unnest(dblink_get_connections()) AS c;
SELECT dblink_get_connections();

-- 22 names cross the 16-bucket initial size: four splits, then removals
-- and re-inserts into the grown table.
SELECT dblink_connect('n' || lpad(i::text, 2, '0'), connection_parameters()) FROM generate_series(1, 20) AS i;
SELECT dblink_get_connections();
SELECT dblink_disconnect('n03');
SELECT dblink_disconnect('n07');
SELECT dblink_connect('n21', connection_parameters());
SELECT dblink_connect('n22', connection_parameters());
SELECT dblink_get_connections();
SELECT dblink_disconnect(c) FROM unnest(dblink_get_connections()) AS c;

SELECT type, name FROM pg_wait_events WHERE name LIKE 'Dblink%' ORDER BY name;

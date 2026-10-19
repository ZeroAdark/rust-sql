CREATE EXTENSION pg_visibility;
CREATE EXTENSION dblink;

-- read_stream_begin_relation rejects another session's temporary table
-- before any block is read (read_stream.c:563-566), so the heap-reading
-- functions raise 0A000 even when the table has no VM fork or no blocks;
-- pg_visibility_map only consults the (absent) VM and returns nothing.
SELECT dblink_connect('other', format('host=%s port=%s dbname=%s user=%s',
  current_setting('unix_socket_directories'), current_setting('port'),
  current_database(), current_user));
SELECT dblink_exec('other', 'CREATE TEMP TABLE t(a int); INSERT INTO t VALUES (1); CREATE TEMP TABLE e(a int)');
CREATE TABLE other_schema AS
  SELECT ns FROM dblink('other', 'SELECT pg_my_temp_schema()::regnamespace::text') AS x(ns text);
DO $$
DECLARE s text; q text;
BEGIN
  SELECT ns INTO s FROM other_schema;
  FOREACH q IN ARRAY ARRAY[
    'SELECT count(*) FROM pg_check_frozen(%L)',
    'SELECT count(*) FROM pg_check_visible(%L)',
    'SELECT count(*) FROM pg_visibility(%L)',
    'SELECT count(*) FROM pg_visibility_map(%L)',
    'SELECT count(*) FROM pg_visibility(%L)',
    'SELECT count(*) FROM pg_visibility_map(%L)']
  LOOP
    BEGIN
      EXECUTE format(q, s || CASE WHEN q LIKE '%map%' OR q LIKE '%pg_visibility(%' THEN '.e' ELSE '.t' END);
      RAISE NOTICE '% -> ok', q;
    EXCEPTION WHEN OTHERS THEN
      RAISE NOTICE '% -> % %', q, SQLSTATE, SQLERRM;
    END;
  END LOOP;
END $$;
SELECT dblink_disconnect('other');
DROP TABLE other_schema;

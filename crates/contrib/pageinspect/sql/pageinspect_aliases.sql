CREATE EXTENSION pageinspect;
CREATE EXTENSION dblink;

CREATE TABLE bt1(i int);
INSERT INTO bt1 SELECT generate_series(1, 100);
CREATE INDEX bt1_i ON bt1(i);

-- gist_page_items resolves an attribute's output function only for a value
-- it prints (gistfuncs.c:309): an empty index loads no other module. The
-- extension is created from another session, so this backend has not
-- loaded hstore itself.
SELECT dblink_connect('other', format('host=%s port=%s dbname=%s user=%s',
  current_setting('unix_socket_directories'), current_setting('port'),
  current_database(), current_user));
SELECT dblink_exec('other', 'CREATE EXTENSION hstore');
SELECT dblink_disconnect('other');
CREATE TABLE gq(p point, h hstore);
CREATE INDEX gq_idx ON gq USING gist(p) INCLUDE (h);
CREATE TABLE mods_before AS SELECT module_name FROM pg_get_loaded_modules();
SELECT count(*) FROM gist_page_items(get_raw_page('gq_idx', 0), 'gq_idx');
SELECT module_name FROM pg_get_loaded_modules()
  EXCEPT SELECT module_name FROM mods_before ORDER BY 1;

-- SRF_FIRSTCALL_INIT precedes the page checks in gin_leafpage_items
-- (ginfuncs.c:197) and the result-type lookup in heap_page_items
-- (heapfuncs.c:154): a scalar-declared entry point fails with 0A000.
CREATE FUNCTION gin_scalar(bytea) RETURNS record
  AS '$libdir/pageinspect', 'gin_leafpage_items' LANGUAGE C STRICT;
SELECT gin_scalar(''::bytea);
SELECT gin_scalar(get_raw_page('bt1', 0));
CREATE FUNCTION heap_scalar(bytea) RETURNS record
  AS '$libdir/pageinspect', 'heap_page_items' LANGUAGE C STRICT;
SELECT heap_scalar(''::bytea);
SELECT heap_scalar(get_raw_page('bt1', 0));

-- bt_page_stats resolves every output column's input function before
-- converting any value (TupleDescGetAttInMetadata, btreefuncs.c:312).
CREATE TYPE sh;
CREATE FUNCTION bts_shell(text, int)
  RETURNS TABLE(blkno date, type sh, live_items int, dead_items int,
                avg_item_size int, page_size int, free_size int, btpo_prev int,
                btpo_next int, btpo_level int, btpo_flags int)
  AS '$libdir/pageinspect', 'bt_page_stats' LANGUAGE C STRICT;
SELECT * FROM bts_shell('bt1_i', 1);

-- bt_metap keeps the index's AccessShareLock through the output conversion
-- (btreefuncs.c:934): a domain CHECK in the declared row type sees it.
CREATE FUNCTION bt1_i_locked(int) RETURNS bool AS $$
  SELECT EXISTS (SELECT 1 FROM pg_locks
                 WHERE relation = 'bt1_i'::regclass AND mode = 'AccessShareLock'
                   AND pid = pg_backend_pid()) $$ LANGUAGE sql;
CREATE DOMAIN lockdom AS int CHECK (bt1_i_locked(VALUE));
CREATE FUNCTION metap_d(text, OUT magic lockdom, OUT version int, OUT root bigint,
                        OUT level bigint, OUT fastroot bigint, OUT fastlevel bigint,
                        OUT last_cleanup_num_delpages bigint,
                        OUT last_cleanup_num_tuples float8, OUT allequalimage bool)
  AS '$libdir/pageinspect', 'bt_metap' LANGUAGE C STRICT;
SELECT * FROM metap_d('bt1_i');

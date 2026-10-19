-- regression: btree/GiST/GIN page checks derive the special size from the page's own
-- pd_pagesize_version, so a damaged page-size field is reported as corruption.
\set VERBOSITY terse
CREATE EXTENSION amcheck;
CREATE EXTENSION pageinspect;
CREATE EXTENSION pg_buffercache;
CREATE TABLE t (k int, a int[]) WITH (autovacuum_enabled = false);
INSERT INTO t SELECT i, ARRAY[i % 7, i] FROM generate_series(1, 200) i;
-- overwrite pd_pagesize_version (bytes 18-19) of one block with 0x7e7f (page size 32256), recompute the checksum, write the file back
CREATE FUNCTION damage(r regclass, blk int) RETURNS text LANGUAGE plpgsql AS $$
DECLARE p bytea; c int; o oid; f bytea := ''; BEGIN
  FOR b IN 0 .. pg_relation_size(r) / 8192 - 1 LOOP
    p := get_raw_page(r::text, b);
    IF b = blk THEN
      p := set_byte(set_byte(p, 18, 127), 19, 126);
      p := set_byte(set_byte(p, 8, 0), 9, 0);
      c := page_checksum(p, b); IF c < 0 THEN c := c + 65536; END IF;
      p := set_byte(set_byte(p, 8, c & 255), 9, c >> 8);
    END IF;
    f := f || p;
  END LOOP;
  o := lo_from_bytea(0, f);
  PERFORM lo_export(o, pg_relation_filepath(r));
  PERFORM lo_unlink(o);
  PERFORM pg_buffercache_evict_relation(r);
  RETURN (SELECT pagesize || '/' || version FROM page_header(get_raw_page(r::text, blk)));
END $$;
SET enable_seqscan = off; SET enable_bitmapscan = off;
CREATE INDEX t_gin ON t USING gin (a);
CHECKPOINT;
SELECT damage('t_gin', 1);
SELECT gin_index_check('t_gin');
DROP INDEX t_gin;
CREATE INDEX t_bt ON t (k);
CHECKPOINT;
SELECT damage('t_bt', 1);
SELECT bt_index_check('t_bt');
SELECT count(*) FROM t WHERE k = 42;
DROP INDEX t_bt;
CREATE INDEX t_gist ON t USING gist (int4range(k, k, '[]'));
CHECKPOINT;
SELECT damage('t_gist', 0);
EXPLAIN (COSTS OFF) SELECT count(*) FROM t WHERE int4range(k, k, '[]') && int4range(40, 45);
SELECT count(*) FROM t WHERE int4range(k, k, '[]') && int4range(40, 45);
DROP INDEX t_gist;
SELECT count(*) FROM t WHERE k = 42;

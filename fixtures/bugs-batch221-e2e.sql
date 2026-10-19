-- Differential fixture for the 2026-09-13 audit bug batch 221
-- (contrib/pageinspect: composite results resolve through the caller's
-- column definition list, the btree SRFs reject a non-set context before
-- touching their arguments, bt_multi_page_stats resolves its result type
-- only when it emits a row, BRIN wrappers wider than C's value arrays are
-- refused). Run via scripts/bugs-batch221-e2e.sh against the frozen expected
-- captured from C PostgreSQL 18.6; the three hand-verified lines are marked
-- (C's SRF state leak drops every other NULL page, and C reads past its
-- fixed BRIN arrays for wider wrappers; see UPSTREAM-BUGS.md).
\pset pager off
CREATE EXTENSION IF NOT EXISTS pageinspect;
CREATE TABLE b221_t(a int);
INSERT INTO b221_t SELECT g FROM generate_series(1, 10) g;
CREATE INDEX b221_idx ON b221_t(a);
CREATE INDEX b221_brin ON b221_t USING brin(a);
-- regression: RECORD wrappers resolve through the
-- column definition list (rsinfo->expectedDesc).
CREATE FUNCTION b221_bt_items(text, int8) RETURNS SETOF record
  LANGUAGE C STRICT AS '$libdir/pageinspect', 'bt_page_items_1_9';
SELECT * FROM b221_bt_items('b221_idx', 1)
  AS x(itemoffset smallint, ctid tid, itemlen smallint, nulls bool, vars bool, data text, dead bool, htid tid, tids tid[]);
CREATE FUNCTION b221_bt_metap(text) RETURNS record
  LANGUAGE C STRICT AS '$libdir/pageinspect', 'bt_metap';
SELECT * FROM b221_bt_metap('b221_idx')
  AS x(magic int4, version int4, root int8, level int8, fastroot int8, fastlevel int8, last_cleanup_num_delpages int8, last_cleanup_num_tuples float8, allequalimage bool);
CREATE FUNCTION b221_bt_stats(text, int8) RETURNS record
  LANGUAGE C STRICT AS '$libdir/pageinspect', 'bt_page_stats_1_9';
SELECT * FROM b221_bt_stats('b221_idx', 1)
  AS x(blkno int8, type "char", live_items int4, dead_items int4, avg_item_size int4, page_size int4, free_size int4, btpo_prev int8, btpo_next int8, btpo_level int8, btpo_flags int4);
-- regression: page_header through a RECORD wrapper, with
-- the pg_lsn, text and int8 spellings of the lsn column.
CREATE FUNCTION b221_ph(bytea) RETURNS record
  LANGUAGE C STRICT AS '$libdir/pageinspect', 'page_header';
SELECT pg_typeof(lsn), lsn IS NOT NULL AS has_lsn, checksum, flags, lower, upper, special, pagesize, version, prune_xid
  FROM b221_ph(get_raw_page('b221_t', 0))
  AS (lsn pg_lsn, checksum int2, flags int2, lower int4, upper int4, special int4, pagesize int4, version int2, prune_xid xid);
SELECT pg_typeof(lsn), lsn ~ '^[0-9A-F]+/[0-9A-F]+$' AS lsn_text, lower, upper
  FROM b221_ph(get_raw_page('b221_t', 0))
  AS (lsn text, checksum int2, flags int2, lower int4, upper int4, special int4, pagesize int4, version int2, prune_xid xid);
SELECT pg_typeof(lsn), lsn > 0 AS lsn_int8, lower, upper
  FROM b221_ph(get_raw_page('b221_t', 0))
  AS (lsn int8, checksum int2, flags int2, lower int4, upper int4, special int4, pagesize int4, version int2, prune_xid xid);
CREATE FUNCTION b221_gin_meta(bytea) RETURNS record
  LANGUAGE C STRICT AS '$libdir/pageinspect', 'gin_metapage_info';
CREATE TABLE b221_g(a int[]);
INSERT INTO b221_g VALUES ('{1,2}');
CREATE INDEX b221_gin ON b221_g USING gin(a);
SELECT * FROM b221_gin_meta(get_raw_page('b221_gin', 0))
  AS (pending_head int8, pending_tail int8, tail_free_size int4, n_pending_pages int8, n_pending_tuples int8, n_total_pages int8, n_entry_pages int8, n_data_pages int8, n_entries int8, version int4);
-- regression: a non-set wrapper hits C's
-- SRF_FIRSTCALL_INIT (0A000) before the page or relation is looked at.
CREATE FUNCTION b221_nonset_bytea(bytea) RETURNS record
  LANGUAGE C STRICT AS '$libdir/pageinspect', 'bt_page_items_bytea';
SELECT b221_nonset_bytea(decode(repeat('00', 8192), 'hex'));
CREATE FUNCTION b221_nonset_items(text, int8) RETURNS record
  LANGUAGE C STRICT AS '$libdir/pageinspect', 'bt_page_items_1_9';
SELECT b221_nonset_items('b221_nonexistent', 1);
CREATE FUNCTION b221_nonset_multi(text, int8, int8) RETURNS record
  LANGUAGE C STRICT AS '$libdir/pageinspect', 'bt_multi_page_stats';
SELECT b221_nonset_multi('b221_nonexistent', 1, 1);
-- regression: bt_multi_page_stats resolves the
-- result type per emitted row, so a zero-row call never checks it.
CREATE FUNCTION b221_multi_int8(text, int8, int8) RETURNS SETOF int8
  LANGUAGE C STRICT AS '$libdir/pageinspect', 'bt_multi_page_stats';
SELECT * FROM b221_multi_int8('b221_idx', 1, 0);
SELECT * FROM b221_multi_int8('b221_idx', 1, 1);
-- regression (hand-verified, C returns 2 rows):
-- every all-zero page fed through a ProjectSet yields its own NULL.
SELECT bt_page_items(p) IS NULL AS is_null
  FROM (VALUES (decode(repeat('00', 8192), 'hex')), (decode(repeat('00', 8192), 'hex')), (decode(repeat('00', 8192), 'hex'))) v(p);
-- regression (hand-verified, C reads past its
-- fixed arrays): BRIN wrappers wider than the extension's definitions.
CREATE FUNCTION b221_brin_items9(IN page bytea, IN index_oid regclass, OUT itemoffset int, OUT blknum int8, OUT attnum int, OUT allnulls bool, OUT hasnulls bool, OUT placeholder bool, OUT empty bool, OUT value text, OUT extra int)
  RETURNS SETOF record LANGUAGE C STRICT AS '$libdir/pageinspect', 'brin_page_items';
SELECT itemoffset, blknum, value, extra FROM b221_brin_items9(get_raw_page('b221_brin', 2), 'b221_brin');
CREATE FUNCTION b221_brin_items7(IN page bytea, IN index_oid regclass, OUT itemoffset int, OUT blknum int8, OUT attnum int, OUT allnulls bool, OUT hasnulls bool, OUT placeholder bool, OUT value text)
  RETURNS SETOF record LANGUAGE C STRICT AS '$libdir/pageinspect', 'brin_page_items';
SELECT * FROM b221_brin_items7(get_raw_page('b221_brin', 2), 'b221_brin');
CREATE FUNCTION b221_brin_meta5(IN page bytea, OUT magic text, OUT version int, OUT pagesperrange int, OUT lastrevmappage int8, OUT extra int)
  RETURNS record LANGUAGE C STRICT AS '$libdir/pageinspect', 'brin_metapage_info';
SELECT * FROM b221_brin_meta5(get_raw_page('b221_brin', 0));
CREATE FUNCTION b221_brin_meta3(IN page bytea, OUT magic text, OUT version int, OUT pagesperrange int)
  RETURNS record LANGUAGE C STRICT AS '$libdir/pageinspect', 'brin_metapage_info';
SELECT * FROM b221_brin_meta3(get_raw_page('b221_brin', 0));
SELECT itemoffset, blknum, value FROM brin_page_items(get_raw_page('b221_brin', 2), 'b221_brin');

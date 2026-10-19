-- Differential fixture for the 2026-09-13 audit bug batch 112
-- (contrib/pageinspect: ItemIdIsValid is a pointer test, bt_page_items and
-- bt_multi_page_stats stream one row per SRF call like C, nine-slot btree
-- output arrays, posting lists bounded by the page only). Run via
-- scripts/bugs-batch112-e2e.sh against the frozen expected captured from C
-- PostgreSQL 18.6 (the tuple_data_split row is hand-verified: C truncates
-- the length to uint16, see UPSTREAM-BUGS.md).
\pset pager off
CREATE EXTENSION IF NOT EXISTS pageinspect;
CREATE TABLE b112_t(a int);
INSERT INTO b112_t SELECT g FROM generate_series(1, 10) g;
CREATE INDEX b112_idx ON b112_t(a);
CREATE TABLE b112_pages AS SELECT get_raw_page('b112_idx', 1) AS leaf;
SELECT lower, upper, special FROM b112_pages, page_header(leaf);
-- regression: line pointer 1 with lp_flags and lp_len
-- cleared (lp_off kept) is still decoded; C's ItemIdIsValid never rejects it.
SELECT i.* FROM b112_pages,
  bt_page_items(set_byte(set_byte(set_byte(leaf, 25, get_byte(leaf, 25) & 127), 26, 0), 27, 0)) i
  LIMIT 2;
-- regression: item 2 with t_info size 0. A FROM-clause
-- call materializes the whole set (error, "-8" = 0 - IndexInfoFindDataOffset),
-- a targetlist call decodes one item per call so LIMIT 1 never reaches item 2.
SELECT i.* FROM b112_pages, bt_page_items(set_byte(set_byte(leaf, 8144 + 6, 0), 8144 + 7, 0)) i LIMIT 1;
SELECT bt_page_items(set_byte(set_byte(leaf, 8144 + 6, 0), 8144 + 7, 0)) FROM b112_pages LIMIT 1;
SELECT bt_page_items(set_byte(set_byte(leaf, 8144 + 6, 0), 8144 + 7, 0)) FROM b112_pages LIMIT 2;
-- regression: a seven-column composite bound to
-- bt_page_items_bytea (C fills nine values, heap_form_tuple consumes natts).
CREATE TYPE b112_rec7 AS (itemoffset smallint, ctid tid, itemlen smallint, nulls bool, vars bool, data text, dead bool);
CREATE FUNCTION b112_bt7(bytea) RETURNS SETOF b112_rec7
  LANGUAGE C STRICT AS '$libdir/pageinspect', 'bt_page_items_bytea';
SELECT i.* FROM b112_pages, b112_bt7(leaf) i LIMIT 2;
-- regression: posting tuple (size 16, posting offset
-- 16) whose two TIDs lie past IndexTupleSize but inside the page image.
CREATE TABLE b112_post AS SELECT
  set_byte(set_byte(set_byte(set_byte(set_byte(set_byte(set_byte(set_byte(set_byte(set_byte(set_byte(set_byte(set_byte(set_byte(
    decode(repeat('00', 8192), 'hex'),
    12, 28), 14, 128), 16, 240), 17, 31), 19, 32), 24, 128), 25, 128), 26, 32),
    130, 16), 132, 2), 133, 32), 134, 16), 135, 32), 8188, 1) AS p;
UPDATE b112_post SET p = set_byte(set_byte(set_byte(set_byte(p, 146, 1), 148, 1), 152, 2), 154, 2);
SELECT i.* FROM b112_post, bt_page_items(p) i;
-- regression / b1#4: GiST line pointers with cleared
-- flags+len, and with lp_len = 8 (C reads the size from t_info only).
CREATE TABLE b112_g(p point);
INSERT INTO b112_g SELECT point(g, g) FROM generate_series(1, 5) g;
CREATE INDEX b112_gidx ON b112_g USING gist(p);
CREATE TABLE b112_gp AS SELECT get_raw_page('b112_gidx', 0) AS pg;
SELECT get_byte(pg, 24), get_byte(pg, 25), get_byte(pg, 26), get_byte(pg, 27) FROM b112_gp;
SELECT i.* FROM b112_gp,
  gist_page_items_bytea(set_byte(set_byte(set_byte(pg, 25, get_byte(pg, 25) & 127), 26, 0), 27, 0)) i
  LIMIT 2;
SELECT i.* FROM b112_gp, gist_page_items(set_byte(set_byte(pg, 26, 16), 27, 0), 'b112_gidx'::regclass) i LIMIT 2;
-- regression: an all-zero line pointer on a hash
-- bucket page decodes the tuple at offset 0 (the page header bytes; pd_lsn
-- zeroed so the row is run-independent).
CREATE TABLE b112_h(a int);
CREATE INDEX b112_hidx ON b112_h USING hash(a);
CREATE TABLE b112_hp AS SELECT overlay(get_raw_page('b112_hidx', 1) PLACING decode(repeat('00', 8), 'hex') FROM 1) AS hp;
SELECT i.* FROM b112_hp,
  hash_page_items(set_byte(set_byte(set_byte(set_byte(set_byte(set_byte(hp, 12, 28), 13, 0), 24, 0), 25, 0), 26, 0), 27, 0)) i;
-- regression: C passes the bytea length as uint16, so
-- 65536 bytes look like 0 and it returns {NULL}; pgrust keeps the real length
-- (deliberate divergence, expected line hand-written).
CREATE TABLE b112_pi_len(a integer);
SELECT tuple_data_split('b112_pi_len'::regclass, decode(repeat('00', 8), 'hex'), 0, 0, NULL);
SELECT tuple_data_split('b112_pi_len'::regclass, decode(repeat('00', 65536), 'hex'), 0, 0, NULL);
-- regression: bt_multi_page_stats keeps its
-- AccessShareLock until the set is exhausted (a LIMIT that stops early leaves
-- it held to transaction end).
BEGIN;
SELECT bt_multi_page_stats('b112_idx', 1, -1) LIMIT 1;
SELECT mode FROM pg_locks WHERE relation = 'b112_idx'::regclass AND pid = pg_backend_pid();
SELECT count(*) FROM bt_multi_page_stats('b112_idx', 1, -1);
COMMIT;
BEGIN;
SELECT * FROM bt_multi_page_stats('b112_idx', 1, -1);
SELECT mode FROM pg_locks WHERE relation = 'b112_idx'::regclass AND pid = pg_backend_pid();
COMMIT;
-- regression: in all-pages mode the relation length
-- is rechecked on every call, so pages appended mid-scan are reported.
CREATE TABLE b112_c(a int);
INSERT INTO b112_c SELECT g FROM generate_series(1, 10) g;
CREATE INDEX b112_cidx ON b112_c(a);
BEGIN;
DECLARE b112_cur CURSOR FOR SELECT (bt_multi_page_stats('b112_cidx', 1, -1)).blkno;
FETCH 1 FROM b112_cur;
INSERT INTO b112_c SELECT g FROM generate_series(11, 2000) g;
FETCH ALL FROM b112_cur;
CLOSE b112_cur;
COMMIT;

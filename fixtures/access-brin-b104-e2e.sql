-- Two-binary differential corpus for the 2026-09-13 bug-inventory batch
-- batch-104-backend-access-brin (C 18.6 oracle vs pgrust). Each leg is one
-- row of the batch (scripts/access-brin-b104-e2e.sh).
\set VERBOSITY verbose
CREATE DATABASE b104e2e TEMPLATE template0 ENCODING 'UTF8';
\c b104e2e
\set VERBOSITY verbose
-- regression: a LANGUAGE internal alias of brin_minmax_opcinfo as support
-- function 1 of a custom opclass builds and scans like int4_minmax_ops
-- (brin.c:1608 dispatches through fmgr, not by pg_proc oid).
CREATE FUNCTION b104_brin_info(internal) RETURNS internal
  LANGUAGE internal IMMUTABLE STRICT AS 'brin_minmax_opcinfo';
CREATE OPERATOR CLASS b104_int4_minmax_ops FOR TYPE int4 USING brin AS
  OPERATOR 1 <, OPERATOR 2 <=, OPERATOR 3 =, OPERATOR 4 >=, OPERATOR 5 >,
  FUNCTION 1 b104_brin_info(internal),
  FUNCTION 2 brin_minmax_add_value(internal, internal, internal, internal),
  FUNCTION 3 brin_minmax_consistent(internal, internal, internal),
  FUNCTION 4 brin_minmax_union(internal, internal, internal),
  STORAGE int4;
CREATE TABLE b104_t(a int4);
INSERT INTO b104_t SELECT g FROM generate_series(1, 5000) g;
CREATE INDEX b104_t_brin ON b104_t USING brin(a b104_int4_minmax_ops) WITH (pages_per_range = 2);
SET enable_seqscan = off;
SELECT count(*) FROM b104_t WHERE a BETWEEN 100 AND 250;
SELECT count(*) FROM b104_t WHERE a = 4321;
INSERT INTO b104_t SELECT g FROM generate_series(5001, 6000) g;
SELECT count(*) FROM b104_t WHERE a > 5990;
SELECT brin_desummarize_range('b104_t_brin', 0);
SELECT brin_summarize_range('b104_t_brin', 0);
SELECT count(*) FROM b104_t WHERE a < 10;
RESET enable_seqscan;
-- regression / #2: pageinspect over a BRIN regular page whose first
-- item's lp_len is shortened to 5 / 8 while the tuple bytes stay intact; the
-- deformer reads the tuple from the page image, not lp_len (brin_tuple.c:582).
CREATE EXTENSION pageinspect;
CREATE TABLE b104_pi(a int);
INSERT INTO b104_pi VALUES (1);
CREATE INDEX b104_pi_idx ON b104_pi USING brin(a);
WITH p AS (SELECT get_raw_page('b104_pi_idx', 2) AS b)
SELECT itemoffset, blknum, attnum, allnulls, hasnulls, placeholder, empty, value
  FROM p CROSS JOIN LATERAL brin_page_items(set_byte(set_byte(b, 26, (get_byte(b, 26) & 1) | 10), 27, 0), 'b104_pi_idx'::regclass);
WITH p AS (SELECT get_raw_page('b104_pi_idx', 2) AS b)
SELECT itemoffset, blknum, attnum, allnulls, hasnulls, placeholder, empty, value
  FROM p CROSS JOIN LATERAL brin_page_items(set_byte(set_byte(b, 26, (get_byte(b, 26) & 1) | 16), 27, 0), 'b104_pi_idx'::regclass);
\c postgres
DROP DATABASE b104e2e;

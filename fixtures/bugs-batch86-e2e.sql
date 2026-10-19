-- Differential fixture for the 2026-09-13 audit bug batch 86
-- (_support/types: BRIN special-space fields read relative to pd_special,
-- GiST / SP-GiST opclass support functions returning SQL NULL raise
-- fmgr's XX000 "function N returned NULL"). Run via
-- scripts/bugs-batch86-e2e.sh against the frozen expected captured from C
-- PostgreSQL 18.6.
\pset pager off
-- regression: brin_page_type reads the type at
-- pd_special + 6 (brin_page.h BrinPageType), not at the block tail. An
-- 8192-byte page declaring a 4096-byte size with pd_special = 4088 passes
-- the special-size check; the type lives at offset 4094.
CREATE EXTENSION IF NOT EXISTS pageinspect;
CREATE TABLE b86_shifted AS SELECT
  set_byte(set_byte(set_byte(set_byte(set_byte(set_byte(set_byte(
    decode(repeat('00', 8192), 'hex'), 14, 1), 16, 248), 17, 15), 18, 4), 19, 16), 4094, 145), 4095, 240) AS meta,
  set_byte(set_byte(set_byte(set_byte(set_byte(set_byte(set_byte(set_byte(set_byte(
    decode(repeat('00', 8192), 'hex'), 14, 1), 16, 248), 17, 15), 18, 4), 19, 16), 4094, 146), 4095, 240), 8190, 147), 8191, 240) AS revmap_tail_regular;
SELECT brin_page_type(meta), brin_page_type(revmap_tail_regular) FROM b86_shifted;
SELECT m.* FROM b86_shifted, brin_metapage_info(meta) m;
SELECT brin_revmap_data(revmap_tail_regular) FROM b86_shifted LIMIT 1;
CREATE TABLE b86_brin (a int);
INSERT INTO b86_brin SELECT g FROM generate_series(1, 1000) g;
CREATE INDEX b86_brin_i ON b86_brin USING brin (a);
SELECT brin_page_type(get_raw_page('b86_brin_i', 0)),
       brin_page_type(get_raw_page('b86_brin_i', 1)),
       brin_page_type(get_raw_page('b86_brin_i', 2));
SELECT * FROM brin_metapage_info(get_raw_page('b86_brin_i', 0));
SELECT count(*) FROM b86_brin WHERE a BETWEEN 10 AND 20;
-- regression / regression: GiST support functions invoked
-- through FunctionCallNColl raise XX000 when they return SQL NULL. A
-- LANGUAGE internal alias of pg_stat_get_function_calls returns NULL for
-- any argument (OID normalized in the messages).
CREATE FUNCTION b86_null5(internal, point, smallint, oid, internal) RETURNS bool
  LANGUAGE internal AS 'pg_stat_get_function_calls';
CREATE FUNCTION b86_nulld(internal, point, smallint, oid, internal) RETURNS float8
  LANGUAGE internal AS 'pg_stat_get_function_calls';
CREATE FUNCTION b86_null2(internal, internal) RETURNS internal
  LANGUAGE internal AS 'pg_stat_get_function_calls';
CREATE FUNCTION b86_null1(internal) RETURNS internal
  LANGUAGE internal AS 'pg_stat_get_function_calls';
CREATE FUNCTION b86_null3(internal, internal, internal) RETURNS internal
  LANGUAGE internal AS 'pg_stat_get_function_calls';
CREATE FUNCTION b86_null3b(box, box, internal) RETURNS internal
  LANGUAGE internal AS 'pg_stat_get_function_calls';
CREATE OPERATOR CLASS b86_gist_nullcons FOR TYPE point USING gist AS
  OPERATOR 1 <<(point, point),
  FUNCTION 1 b86_null5(internal, point, smallint, oid, internal),
  FUNCTION 2 gist_box_union(internal, internal),
  FUNCTION 3 gist_point_compress(internal),
  FUNCTION 5 gist_box_penalty(internal, internal, internal),
  FUNCTION 6 gist_box_picksplit(internal, internal),
  FUNCTION 7 gist_box_same(box, box, internal),
  STORAGE box;
CREATE OPERATOR CLASS b86_gist_nulldist FOR TYPE point USING gist AS
  OPERATOR 15 <->(point, point) FOR ORDER BY float_ops,
  FUNCTION 1 gist_point_consistent(internal, point, smallint, oid, internal),
  FUNCTION 2 gist_box_union(internal, internal),
  FUNCTION 3 gist_point_compress(internal),
  FUNCTION 5 gist_box_penalty(internal, internal, internal),
  FUNCTION 6 gist_box_picksplit(internal, internal),
  FUNCTION 7 gist_box_same(box, box, internal),
  FUNCTION 8 b86_nulld(internal, point, smallint, oid, internal),
  STORAGE box;
CREATE OPERATOR CLASS b86_gist_nullsplit FOR TYPE point USING gist AS
  OPERATOR 1 <<(point, point),
  FUNCTION 1 gist_point_consistent(internal, point, smallint, oid, internal),
  FUNCTION 2 gist_box_union(internal, internal),
  FUNCTION 3 gist_point_compress(internal),
  FUNCTION 5 gist_box_penalty(internal, internal, internal),
  FUNCTION 6 b86_null2(internal, internal),
  FUNCTION 7 gist_box_same(box, box, internal),
  STORAGE box;
CREATE OPERATOR CLASS b86_gist_nullcompress FOR TYPE point USING gist AS
  OPERATOR 1 <<(point, point),
  FUNCTION 1 gist_point_consistent(internal, point, smallint, oid, internal),
  FUNCTION 2 gist_box_union(internal, internal),
  FUNCTION 3 b86_null1(internal),
  FUNCTION 5 gist_box_penalty(internal, internal, internal),
  FUNCTION 6 gist_box_picksplit(internal, internal),
  FUNCTION 7 gist_box_same(box, box, internal),
  STORAGE box;
CREATE OPERATOR CLASS b86_gist_nullpenalty FOR TYPE point USING gist AS
  OPERATOR 1 <<(point, point),
  FUNCTION 1 gist_point_consistent(internal, point, smallint, oid, internal),
  FUNCTION 2 gist_box_union(internal, internal),
  FUNCTION 3 gist_point_compress(internal),
  FUNCTION 5 b86_null3(internal, internal, internal),
  FUNCTION 6 gist_box_picksplit(internal, internal),
  FUNCTION 7 gist_box_same(box, box, internal),
  STORAGE box;
CREATE OPERATOR CLASS b86_gist_nullunion FOR TYPE point USING gist AS
  OPERATOR 1 <<(point, point),
  FUNCTION 1 gist_point_consistent(internal, point, smallint, oid, internal),
  FUNCTION 2 b86_null2(internal, internal),
  FUNCTION 3 gist_point_compress(internal),
  FUNCTION 5 gist_box_penalty(internal, internal, internal),
  FUNCTION 6 gist_box_picksplit(internal, internal),
  FUNCTION 7 gist_box_same(box, box, internal),
  STORAGE box;
CREATE OPERATOR CLASS b86_gist_nullsame FOR TYPE point USING gist AS
  OPERATOR 1 <<(point, point),
  FUNCTION 1 gist_point_consistent(internal, point, smallint, oid, internal),
  FUNCTION 2 gist_box_union(internal, internal),
  FUNCTION 3 gist_point_compress(internal),
  FUNCTION 5 gist_box_penalty(internal, internal, internal),
  FUNCTION 6 gist_box_picksplit(internal, internal),
  FUNCTION 7 b86_null3b(box, box, internal),
  STORAGE box;
CREATE TABLE b86_gp (p point);
INSERT INTO b86_gp VALUES ('(1,1)'), ('(2,2)');
CREATE INDEX b86_gp_cons ON b86_gp USING gist (p b86_gist_nullcons);
CREATE INDEX b86_gp_dist ON b86_gp USING gist (p b86_gist_nulldist);
SET enable_seqscan = off;
SET enable_bitmapscan = off;
DO $$ BEGIN
  PERFORM * FROM b86_gp WHERE p << '(5,5)';
EXCEPTION WHEN OTHERS THEN
  RAISE NOTICE '% %', SQLSTATE, regexp_replace(SQLERRM, '\d+', 'N');
END $$;
DO $$ BEGIN
  PERFORM * FROM b86_gp ORDER BY p <-> '(0,0)' LIMIT 1;
EXCEPTION WHEN OTHERS THEN
  RAISE NOTICE '% %', SQLSTATE, regexp_replace(SQLERRM, '\d+', 'N');
END $$;
RESET enable_seqscan;
RESET enable_bitmapscan;
CREATE TABLE b86_gps (p point);
INSERT INTO b86_gps SELECT point(g, g) FROM generate_series(1, 2000) g;
DO $$ BEGIN
  CREATE INDEX b86_gps_split ON b86_gps USING gist (p b86_gist_nullsplit);
EXCEPTION WHEN OTHERS THEN
  RAISE NOTICE '% %', SQLSTATE, regexp_replace(SQLERRM, '\d+', 'N');
END $$;
DO $$ BEGIN
  CREATE INDEX b86_gps_compress ON b86_gps USING gist (p b86_gist_nullcompress);
EXCEPTION WHEN OTHERS THEN
  RAISE NOTICE '% %', SQLSTATE, regexp_replace(SQLERRM, '\d+', 'N');
END $$;
DO $$ BEGIN
  CREATE INDEX b86_gps_penalty ON b86_gps USING gist (p b86_gist_nullpenalty);
EXCEPTION WHEN OTHERS THEN
  RAISE NOTICE '% %', SQLSTATE, regexp_replace(SQLERRM, '\d+', 'N');
END $$;
DO $$ BEGIN
  CREATE INDEX b86_gps_union ON b86_gps USING gist (p b86_gist_nullunion);
EXCEPTION WHEN OTHERS THEN
  RAISE NOTICE '% %', SQLSTATE, regexp_replace(SQLERRM, '\d+', 'N');
END $$;
DO $$ BEGIN
  CREATE INDEX b86_gps_same ON b86_gps USING gist (p b86_gist_nullsame);
EXCEPTION WHEN OTHERS THEN
  RAISE NOTICE '% %', SQLSTATE, regexp_replace(SQLERRM, '\d+', 'N');
END $$;
SELECT relname FROM pg_class WHERE relname LIKE 'b86_gps_%' ORDER BY 1;
-- regression: SP-GiST choose / picksplit / compress and the
-- scan-side inner / leaf consistent functions take the same check.
CREATE FUNCTION b86_spgnull2(internal, internal) RETURNS void
  LANGUAGE internal AS 'pg_stat_get_function_calls';
CREATE FUNCTION b86_spgnull2b(internal, internal) RETURNS bool
  LANGUAGE internal AS 'pg_stat_get_function_calls';
CREATE FUNCTION b86_spgnullc(polygon) RETURNS box
  LANGUAGE internal AS 'pg_stat_get_function_calls';
CREATE OPERATOR CLASS b86_spg_nullchoose FOR TYPE point USING spgist AS
  OPERATOR 1 <<(point, point),
  FUNCTION 1 spg_quad_config(internal, internal),
  FUNCTION 2 b86_spgnull2(internal, internal),
  FUNCTION 3 spg_quad_picksplit(internal, internal),
  FUNCTION 4 spg_quad_inner_consistent(internal, internal),
  FUNCTION 5 spg_quad_leaf_consistent(internal, internal);
CREATE OPERATOR CLASS b86_spg_nullsplit FOR TYPE point USING spgist AS
  OPERATOR 1 <<(point, point),
  FUNCTION 1 spg_quad_config(internal, internal),
  FUNCTION 2 spg_quad_choose(internal, internal),
  FUNCTION 3 b86_spgnull2(internal, internal),
  FUNCTION 4 spg_quad_inner_consistent(internal, internal),
  FUNCTION 5 spg_quad_leaf_consistent(internal, internal);
CREATE OPERATOR CLASS b86_spg_nullinner FOR TYPE point USING spgist AS
  OPERATOR 1 <<(point, point),
  FUNCTION 1 spg_quad_config(internal, internal),
  FUNCTION 2 spg_quad_choose(internal, internal),
  FUNCTION 3 spg_quad_picksplit(internal, internal),
  FUNCTION 4 b86_spgnull2(internal, internal),
  FUNCTION 5 spg_quad_leaf_consistent(internal, internal);
CREATE OPERATOR CLASS b86_spg_nullleaf FOR TYPE point USING spgist AS
  OPERATOR 1 <<(point, point),
  FUNCTION 1 spg_quad_config(internal, internal),
  FUNCTION 2 spg_quad_choose(internal, internal),
  FUNCTION 3 spg_quad_picksplit(internal, internal),
  FUNCTION 4 spg_quad_inner_consistent(internal, internal),
  FUNCTION 5 b86_spgnull2b(internal, internal);
CREATE OPERATOR CLASS b86_spg_nullcompress FOR TYPE polygon USING spgist AS
  OPERATOR 1 <<(polygon, polygon),
  FUNCTION 1 spg_bbox_quad_config(internal, internal),
  FUNCTION 2 spg_box_quad_choose(internal, internal),
  FUNCTION 3 spg_box_quad_picksplit(internal, internal),
  FUNCTION 4 spg_box_quad_inner_consistent(internal, internal),
  FUNCTION 5 spg_box_quad_leaf_consistent(internal, internal),
  FUNCTION 6 b86_spgnullc(polygon),
  STORAGE box;
CREATE TABLE b86_sp (p point);
INSERT INTO b86_sp SELECT point(g, g) FROM generate_series(1, 2000) g;
DO $$ BEGIN
  CREATE INDEX b86_sp_choose ON b86_sp USING spgist (p b86_spg_nullchoose);
EXCEPTION WHEN OTHERS THEN
  RAISE NOTICE '% %', SQLSTATE, regexp_replace(SQLERRM, '\d+', 'N');
END $$;
DO $$ BEGIN
  CREATE INDEX b86_sp_split ON b86_sp USING spgist (p b86_spg_nullsplit);
EXCEPTION WHEN OTHERS THEN
  RAISE NOTICE '% %', SQLSTATE, regexp_replace(SQLERRM, '\d+', 'N');
END $$;
CREATE INDEX b86_sp_inner ON b86_sp USING spgist (p b86_spg_nullinner);
CREATE INDEX b86_sp_leaf ON b86_sp USING spgist (p b86_spg_nullleaf);
SET enable_seqscan = off;
SET enable_bitmapscan = off;
DO $$ BEGIN
  PERFORM * FROM b86_sp WHERE p << '(5,5)';
EXCEPTION WHEN OTHERS THEN
  RAISE NOTICE '% %', SQLSTATE, regexp_replace(SQLERRM, '\d+', 'N');
END $$;
DROP INDEX b86_sp_inner;
DO $$ BEGIN
  PERFORM * FROM b86_sp WHERE p << '(5,5)';
EXCEPTION WHEN OTHERS THEN
  RAISE NOTICE '% %', SQLSTATE, regexp_replace(SQLERRM, '\d+', 'N');
END $$;
RESET enable_seqscan;
RESET enable_bitmapscan;
CREATE TABLE b86_spc (p polygon);
INSERT INTO b86_spc VALUES ('((1,1),(2,2),(3,1))');
DO $$ BEGIN
  CREATE INDEX b86_spc_compress ON b86_spc USING spgist (p b86_spg_nullcompress);
EXCEPTION WHEN OTHERS THEN
  RAISE NOTICE '% %', SQLSTATE, regexp_replace(SQLERRM, '\d+', 'N');
END $$;
SELECT relname FROM pg_class WHERE relname LIKE 'b86_sp%' AND relkind = 'i' ORDER BY 1;

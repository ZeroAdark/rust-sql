-- Two-binary differential corpus for the 2026-09-13 bug-inventory batch
-- batch-191-backend-commands-analyze (C 18.6 oracle vs pgrust). Each leg is
-- one row of the batch (commands/analyze: range/multirange typanalyze setup,
-- compute_scalar_stats duplicate grouping, typanalyze aliases).
\set VERBOSITY verbose
CREATE DATABASE b191e2e TEMPLATE template0 ENCODING 'UTF8';
\c b191e2e
\set VERBOSITY verbose
-- regression: range_typanalyze / multirange_typanalyze
-- validate the column type at setup (rangetypes.c:1776 / multirangetypes.c:559),
-- with and without sample rows.
CREATE TYPE b191_mr;
CREATE FUNCTION b191_mr_in(cstring) RETURNS b191_mr LANGUAGE internal IMMUTABLE STRICT AS 'int4in';
CREATE FUNCTION b191_mr_out(b191_mr) RETURNS cstring LANGUAGE internal IMMUTABLE STRICT AS 'int4out';
CREATE TYPE b191_mr (INPUT = b191_mr_in, OUTPUT = b191_mr_out, LIKE = int4, ANALYZE = multirange_typanalyze);
CREATE TABLE b191_tmr (x b191_mr);
DO $$ BEGIN ANALYZE b191_tmr; EXCEPTION WHEN OTHERS THEN RAISE NOTICE '% %', SQLSTATE, regexp_replace(SQLERRM, '\d+', 'N'); END $$;
INSERT INTO b191_tmr VALUES ('1'), ('2');
DO $$ BEGIN ANALYZE b191_tmr; EXCEPTION WHEN OTHERS THEN RAISE NOTICE '% %', SQLSTATE, regexp_replace(SQLERRM, '\d+', 'N'); END $$;
CREATE TYPE b191_r;
CREATE FUNCTION b191_r_in(cstring) RETURNS b191_r LANGUAGE internal IMMUTABLE STRICT AS 'int4in';
CREATE FUNCTION b191_r_out(b191_r) RETURNS cstring LANGUAGE internal IMMUTABLE STRICT AS 'int4out';
CREATE TYPE b191_r (INPUT = b191_r_in, OUTPUT = b191_r_out, LIKE = int4, ANALYZE = range_typanalyze);
CREATE TABLE b191_tr (x b191_r);
DO $$ BEGIN ANALYZE b191_tr; EXCEPTION WHEN OTHERS THEN RAISE NOTICE '% %', SQLSTATE, regexp_replace(SQLERRM, '\d+', 'N'); END $$;
INSERT INTO b191_tr VALUES ('1'), ('2');
DO $$ BEGIN ANALYZE b191_tr; EXCEPTION WHEN OTHERS THEN RAISE NOTICE '% %', SQLSTATE, regexp_replace(SQLERRM, '\d+', 'N'); END $$;
-- A real range / multirange column still analyzes.
CREATE TABLE b191_ok (r int4range, m int4multirange);
INSERT INTO b191_ok SELECT int4range(i, i + 2), int4multirange(int4range(i, i + 1)) FROM generate_series(1, 20) i;
ANALYZE b191_ok;
SELECT attname, n_distinct, range_length_histogram, range_empty_frac FROM pg_stats WHERE tablename = 'b191_ok' ORDER BY attname;
-- regression: duplicate groups come from compare_scalars' tupnoLink,
-- so an intransitive comparator (cube_cmp treats NaN as equal to any coordinate)
-- groups exactly as C's sort does.
CREATE EXTENSION cube;
CREATE TABLE b191_cube (c cube);
INSERT INTO b191_cube VALUES ('(2)'), ('(NaN)'), ('(1)'), ('(0)'), ('(0)'), ('(0)'), ('(0)');
ANALYZE b191_cube;
SELECT n_distinct, most_common_vals, most_common_freqs, histogram_bounds FROM pg_stats WHERE tablename = 'b191_cube';
CREATE TABLE b191_cube2 (c cube);
INSERT INTO b191_cube2 VALUES ('(1)'), ('(3)'), ('(NaN)'), ('(2)'), ('(3)'), ('(NaN)'), ('(1)'), ('(5)'), ('(4)'), ('(NaN, 1)'), ('(2)'), ('(9)');
ANALYZE b191_cube2;
SELECT n_distinct, most_common_vals, most_common_freqs, histogram_bounds FROM pg_stats WHERE tablename = 'b191_cube2';
-- regression / regression: a LANGUAGE internal
-- alias of ts_typanalyze dispatches like the builtin (column and expression).
CREATE FUNCTION b191_tsan(internal) RETURNS boolean LANGUAGE internal AS 'ts_typanalyze';
CREATE TYPE b191_ts;
CREATE FUNCTION b191_ts_in(cstring) RETURNS b191_ts LANGUAGE internal IMMUTABLE STRICT AS 'tsvectorin';
CREATE FUNCTION b191_ts_out(b191_ts) RETURNS cstring LANGUAGE internal IMMUTABLE STRICT AS 'tsvectorout';
CREATE TYPE b191_ts (INPUT = b191_ts_in, OUTPUT = b191_ts_out, LIKE = tsvector, ANALYZE = b191_tsan);
CREATE TABLE b191_tts (x b191_ts);
INSERT INTO b191_tts VALUES ('a b c'), ('a b'), ('a');
ANALYZE b191_tts;
SELECT attname, most_common_elems, most_common_elem_freqs FROM pg_stats WHERE tablename = 'b191_tts';
ALTER TYPE tsvector SET (ANALYZE = b191_tsan);
CREATE TABLE b191_tsv (x tsvector);
INSERT INTO b191_tsv VALUES ('a b c'), ('a b'), ('a');
CREATE STATISTICS b191_stx ON (x || x) FROM b191_tsv;
ANALYZE b191_tsv;
SELECT most_common_elems, most_common_elem_freqs FROM pg_stats_ext_exprs WHERE statistics_name = 'b191_stx';

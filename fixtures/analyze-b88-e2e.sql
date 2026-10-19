-- Two-binary differential corpus for the 2026-09-13 bug-inventory batch
-- batch-88-backend-commands-analyze (C 18.6 oracle vs pgrust). Each leg is
-- one row of the batch (scripts/analyze-b88-e2e.sh). Every table stays
-- within the 300-row sample of statistics target 1 so the whole table is
-- the sample and the catalog bytes are deterministic.
CREATE DATABASE b88e2e TEMPLATE template0 ENCODING 'UTF8';
\c b88e2e
\set VERBOSITY verbose
-- regression: the length histogram is sorted with C's
-- qsort under float8_qsort_cmp, whose NaN arm is inconsistent: {NaN,1}.
CREATE TABLE b88_rnan (r numrange);
INSERT INTO b88_rnan VALUES ('[0,1)'), ('[0,NaN)');
ANALYZE b88_rnan;
SELECT stakind1, stavalues1, stakind2, stavalues2 FROM pg_statistic WHERE starelid = 'b88_rnan'::regclass AND staattnum = 1;
-- regression: equal-frequency MCELEM candidates are truncated
-- in dynahash bucket order + pg qsort order (11 candidates, 10 kept).
CREATE TABLE b88_a1(v int[]);
ALTER TABLE b88_a1 ALTER COLUMN v SET STATISTICS 1;
INSERT INTO b88_a1 VALUES (ARRAY[1,2,3,4,5,6,7,8,9,10,11]);
ANALYZE b88_a1;
SELECT most_common_elems, most_common_elem_freqs FROM pg_stats WHERE tablename = 'b88_a1';
CREATE TABLE b88_a2(v int[]);
ALTER TABLE b88_a2 ALTER COLUMN v SET STATISTICS 1;
INSERT INTO b88_a2 VALUES (ARRAY[11,10,9,8,7,6,5,4,3,2,1]);
ANALYZE b88_a2;
SELECT most_common_elems, most_common_elem_freqs FROM pg_stats WHERE tablename = 'b88_a2';
CREATE TABLE b88_a3(v text[]);
ALTER TABLE b88_a3 ALTER COLUMN v SET STATISTICS 1;
INSERT INTO b88_a3 SELECT array_agg('e' || g) FROM generate_series(1,25) g;
ANALYZE b88_a3;
SELECT most_common_elems, most_common_elem_freqs FROM pg_stats WHERE tablename = 'b88_a3';
-- regression: 3000 sampled elements at bucket width 1428
-- prune twice (HASH_REMOVE + pfree) and split the table past 16 buckets.
CREATE TABLE b88_a4(v int[]);
ALTER TABLE b88_a4 ALTER COLUMN v SET STATISTICS 1;
INSERT INTO b88_a4 SELECT ARRAY[g % 50, g % 7, 1000 + g, g % 3, g % 11, g % 13, g % 17, g % 19, g % 23, g % 29] FROM generate_series(1, 300) g;
ANALYZE b88_a4;
SELECT most_common_elems, most_common_elem_freqs, elem_count_histogram FROM pg_stats WHERE tablename = 'b88_a4';
-- regression: the same truncation for lexemes; the
-- second table's per-row lexemes are pruned away at each bucket boundary.
CREATE TABLE b88_ts1(v tsvector);
ALTER TABLE b88_ts1 ALTER COLUMN v SET STATISTICS 1;
INSERT INTO b88_ts1 SELECT (SELECT string_agg('lex' || g, ' ') FROM generate_series(1,100) g)::tsvector FROM generate_series(1,100);
ANALYZE b88_ts1;
SELECT most_common_elems, most_common_elem_freqs FROM pg_stats WHERE tablename = 'b88_ts1';
CREATE TABLE b88_ts2(v tsvector);
ALTER TABLE b88_ts2 ALTER COLUMN v SET STATISTICS 1;
INSERT INTO b88_ts2 SELECT ((SELECT string_agg('common' || c, ' ') FROM generate_series(1,20) c) || ' ' || (SELECT string_agg('row' || r || 'u' || u, ' ') FROM generate_series(1,30) u))::tsvector FROM generate_series(1,300) r;
ANALYZE b88_ts2;
SELECT most_common_elems, most_common_elem_freqs FROM pg_stats WHERE tablename = 'b88_ts2';
-- regression: 24 one-page children at targrows 300 get
-- rint(12.5) = 12 rows each (288 total); the null child weighs 12/288.
SET default_statistics_target = 1;
CREATE TABLE b88_p(id int, v int) PARTITION BY LIST (id);
DO $$ BEGIN FOR i IN 1..24 LOOP EXECUTE format('CREATE TABLE b88_p%s PARTITION OF b88_p FOR VALUES IN (%s)', i, i); EXECUTE format('INSERT INTO b88_p SELECT %s, CASE WHEN %s = 24 THEN NULL ELSE g END FROM generate_series(1,100) g', i, i); END LOOP; END $$;
ANALYZE b88_p;
SELECT null_frac FROM pg_stats WHERE tablename = 'b88_p' AND attname = 'v' AND inherited;
RESET default_statistics_target;
-- regression / regression: an empty-only
-- range column stores NULL stavalues (witness legs, same on both engines).
CREATE TABLE b88_t2(r int4range);
INSERT INTO b88_t2 VALUES ('empty');
ANALYZE b88_t2;
SELECT stakind1, stavalues1 IS NULL AS v1null, stanumbers1 FROM pg_statistic WHERE starelid = 'b88_t2'::regclass;
CREATE TABLE b88_t3(a int);
INSERT INTO b88_t3 VALUES (1),(2);
CREATE STATISTICS b88_s3 ON (int4range(a,a)) FROM b88_t3;
ANALYZE b88_t3;
SELECT (stxdexpr[1]).stakind1, (stxdexpr[1]).stavalues1 IS NULL AS v1null FROM pg_statistic_ext_data WHERE stxoid = (SELECT oid FROM pg_statistic_ext WHERE stxname = 'b88_s3') AND NOT stxdinherit;
-- regression: expression results now live in a
-- per-expression context; the serialized stats are unchanged.
CREATE TABLE b88_t4(a int);
INSERT INTO b88_t4 SELECT g FROM generate_series(1, 200) g;
CREATE STATISTICS b88_s4 ON (a % 10), (repeat(a::text, 3)) FROM b88_t4;
ANALYZE b88_t4;
SELECT expr, n_distinct, most_common_vals, histogram_bounds FROM pg_stats_ext_exprs WHERE statistics_name = 'b88_s4' ORDER BY expr;
-- regression: ANALYZE runs with cost accounting armed.
SET vacuum_cost_delay = 1;
SET vacuum_cost_limit = 1;
ANALYZE b88_t4;
SELECT n_distinct FROM pg_stats WHERE tablename = 'b88_t4' AND attname = 'a';
RESET vacuum_cost_delay;
RESET vacuum_cost_limit;

-- regression: extended statistics over output-function expressions (int::text,
-- date::text) keep a copy of every sampled row's value, as C's datumCopy.
CREATE TABLE s (a int, b int, c text, d date);
INSERT INTO s SELECT x, x % 10, 'c' || x % 5, date '2024-01-01' + x % 20 FROM generate_series(1, 500) x;
CREATE STATISTICS s_ab ON (a::text), (b::text) FROM s;
CREATE STATISTICS s_bc ON (b::text), c FROM s;
CREATE STATISTICS s_db (ndistinct) ON (d::text), b FROM s;
CREATE STATISTICS s_ctl ON (a + 0), (b + 0) FROM s;
CREATE STATISTICS s_ctl2 ON (a::text || ''), (b::text || '') FROM s;
ANALYZE s;
SELECT statistics_name, n_distinct, dependencies FROM pg_stats_ext WHERE tablename = 's' ORDER BY 1;
SELECT statistics_name, array_length(most_common_freqs, 1) AS mcv_items FROM pg_stats_ext WHERE tablename = 's' ORDER BY 1;
SELECT statistics_name, expr, n_distinct FROM pg_stats_ext_exprs WHERE tablename = 's' ORDER BY 1, 2;
SELECT count(DISTINCT (a::text, b::text)) AS real_ab, count(DISTINCT (b::text, c)) AS real_bc, count(DISTINCT (d::text, b)) AS real_db FROM s;

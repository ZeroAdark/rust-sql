\set VERBOSITY verbose
CREATE ACCESS METHOD pgrcolumnar2 TYPE TABLE HANDLER heap_tableam_handler;
CREATE TABLE percentile_integer_order(g smallint, s smallint, i integer, b bigint) USING pgrcolumnar2;

-- Empty groups do not finalize either valid or invalid percentile fractions.
SELECT g, percentile_cont(2.0) WITHIN GROUP (ORDER BY i) FROM percentile_integer_order WHERE i < 0 GROUP BY g;
SELECT g, percentile_cont(0.5) WITHIN GROUP (ORDER BY i) FROM percentile_integer_order WHERE i < 0 GROUP BY g;

COPY percentile_integer_order FROM STDIN;
1	1	10	1099511627777
1	2	20	1099511627778
1	3	30	1099511627779
1	4	40	1099511627780
2	-4	-40	-1099511627780
2	-3	-30	-1099511627779
2	-2	-20	-1099511627778
2	-1	-10	-1099511627777
\.

-- Integer inputs are implicitly cast to float8 before PostgreSQL orders them.
SELECT g, percentile_cont(0.25) WITHIN GROUP (ORDER BY s) FROM percentile_integer_order GROUP BY g ORDER BY g;
SELECT g, percentile_cont(0.25) WITHIN GROUP (ORDER BY i) FROM percentile_integer_order GROUP BY g ORDER BY g;
SELECT g, percentile_cont(0.25) WITHIN GROUP (ORDER BY b) FROM percentile_integer_order GROUP BY g ORDER BY g;
SELECT g, percentile_cont(0.25) WITHIN GROUP (ORDER BY s DESC) FROM percentile_integer_order GROUP BY g ORDER BY g;
SELECT g, percentile_cont(0.25) WITHIN GROUP (ORDER BY i DESC) FROM percentile_integer_order GROUP BY g ORDER BY g;
SELECT g, percentile_cont(0.25) WITHIN GROUP (ORDER BY b DESC) FROM percentile_integer_order GROUP BY g ORDER BY g;
SELECT percentile_cont(0.5) WITHIN GROUP (ORDER BY i) FROM percentile_integer_order;
SELECT g, percentile_cont(2.0) WITHIN GROUP (ORDER BY i) FROM percentile_integer_order WHERE i > 100 GROUP BY g;
SELECT g, percentile_cont(2.0) WITHIN GROUP (ORDER BY i) FROM percentile_integer_order GROUP BY g;
DROP TABLE percentile_integer_order;

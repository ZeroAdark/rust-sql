\set VERBOSITY verbose
DO $$ BEGIN IF NOT EXISTS (SELECT 1 FROM pg_am WHERE amname = 'pgrcolumnar2') THEN CREATE ACCESS METHOD pgrcolumnar2 TYPE TABLE HANDLER heap_tableam_handler; END IF; END $$;
CREATE TABLE gv (a int NOT NULL, b int2 NOT NULL, x int8 NOT NULL) USING pgrcolumnar2;
COPY gv FROM stdin;
1	1	-1
1	2	5
2	1	7
2	2	9
\.
-- controls: the variance family under a plain GROUP BY, and sum/avg under ROLLUP
SELECT a, stddev(x), var_pop(b) FROM gv GROUP BY a ORDER BY a;
SELECT a, sum(x), avg(x) FROM gv GROUP BY ROLLUP (a) ORDER BY a NULLS LAST;
-- decisive: the variance family under grouping sets
SELECT a, stddev(x) FROM gv GROUP BY ROLLUP (a) ORDER BY a NULLS LAST;
SELECT a, var_pop(b), variance(x) FROM gv GROUP BY CUBE (a) ORDER BY a NULLS LAST;
SELECT a, sum(x), stddev_pop(b) FROM gv GROUP BY GROUPING SETS ((a), ()) ORDER BY a NULLS LAST;

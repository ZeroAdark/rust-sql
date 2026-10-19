\set VERBOSITY verbose
DO $$ BEGIN IF NOT EXISTS (SELECT 1 FROM pg_am WHERE amname = 'pgrcolumnar2') THEN CREATE ACCESS METHOD pgrcolumnar2 TYPE TABLE HANDLER heap_tableam_handler; END IF; END $$;
CREATE TABLE gs (a int NOT NULL, v int8 NOT NULL, n numeric(8,2) NOT NULL) USING pgrcolumnar2;
COPY gs FROM stdin;
1	10	1.50
1	20	2.25
2	30	-3.00
\.
-- controls: plain GROUP BY over the numeric column, and ROLLUP over the int8 column
SELECT a, sum(n) FROM gs GROUP BY a ORDER BY a;
SELECT a, sum(v) FROM gs GROUP BY ROLLUP (a) ORDER BY a NULLS LAST;
-- decisive: grouping sets with an aggregate over the numeric column
SELECT a, sum(n) FROM gs GROUP BY ROLLUP (a) ORDER BY a NULLS LAST;
SELECT a, avg(n), min(n), max(n) FROM gs GROUP BY CUBE (a) ORDER BY a NULLS LAST;
SELECT a, count(*), sum(n) FROM gs GROUP BY GROUPING SETS ((a), ()) ORDER BY a NULLS LAST;

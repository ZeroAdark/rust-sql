\set VERBOSITY verbose
DO $$ BEGIN IF NOT EXISTS (SELECT 1 FROM pg_am WHERE amname = 'pgrcolumnar2') THEN CREATE ACCESS METHOD pgrcolumnar2 TYPE TABLE HANDLER heap_tableam_handler; END IF; END $$;
CREATE TABLE s (a int NOT NULL, v int NOT NULL, b int2 NOT NULL, c int8 NOT NULL) USING pgrcolumnar2;
COPY s FROM stdin;
1	10	1	5
2	-3	2	40
3	4	3	100
\.
-- controls: the same scalar subqueries with max/min/count, and sum(v) on its own
SELECT sum(v) FROM s;
SELECT count(*) FROM s WHERE c >= (SELECT min(c) FROM s);
SELECT count(*) FROM s WHERE c > (SELECT count(*) FROM s);
-- decisive: a scalar subquery sum over an int4 / int2 column used in the WHERE clause
SELECT count(*) FROM s WHERE c > (SELECT sum(v) FROM s);
SELECT count(*) FROM s WHERE v < (SELECT sum(v) FROM s);
SELECT a, c FROM s WHERE c > (SELECT sum(b) FROM s) ORDER BY a;
-- face: the same sum answer as the seed of a recursive CTE
WITH RECURSIVE r(k) AS (SELECT sum(v) FROM s UNION ALL SELECT k + 1 FROM r WHERE k < 13) SELECT * FROM r;
WITH RECURSIVE r(k) AS (SELECT count(*) FROM s UNION ALL SELECT k + 1 FROM r WHERE k < 5) SELECT * FROM r;

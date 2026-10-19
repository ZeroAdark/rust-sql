\set VERBOSITY verbose
DO $$ BEGIN IF NOT EXISTS (SELECT 1 FROM pg_am WHERE amname = 'pgrcolumnar2') THEN CREATE ACCESS METHOD pgrcolumnar2 TYPE TABLE HANDLER heap_tableam_handler; END IF; END $$;
CREATE TABLE e1 (id int NOT NULL, a int NOT NULL) USING pgrcolumnar2;
CREATE TABLE e2 (k int NOT NULL, w int8 NOT NULL) USING pgrcolumnar2;
CREATE TABLE e3 (k int NOT NULL, w int8 NOT NULL) USING pgrcolumnar2;
COPY e1 FROM stdin;
1	10
2	20
\.
-- e2 shares no key with e1; e3 matches a = 10
COPY e2 FROM stdin;
7	1
8	2
\.
COPY e3 FROM stdin;
10	1
11	2
\.
ANALYZE e1; ANALYZE e2; ANALYZE e3;
\pset null '<null>'
-- controls: other aggregates over the same empty join, and the conditional sum over a join that matches
SELECT sum(a) AS sum_a, count(*) AS n FROM e1 x JOIN e2 y ON x.a = y.k;
SELECT sum(CASE WHEN a > 15 THEN 1 ELSE 0 END) AS hits, count(*) AS n FROM e1 x JOIN e3 y ON x.a = y.k;
-- decisive: sum(CASE WHEN ... THEN 1 ELSE 0 END) over no input rows must be NULL
SELECT sum(CASE WHEN a > 0 THEN 1 ELSE 0 END) AS hits FROM e1 x JOIN e2 y ON x.a = y.k;
SELECT sum(CASE WHEN y.w > 0 THEN 1 ELSE 0 END) AS hits, count(*) AS n FROM e1 x JOIN e2 y ON x.a = y.k;
SELECT sum(CASE WHEN a > 0 THEN 1 ELSE 0 END) AS hits, count(*) AS n FROM e1 x WHERE EXISTS (SELECT 1 FROM e2 z WHERE z.k = x.a);
SELECT sum(CASE WHEN a > 0 THEN 1 ELSE 0 END) AS hits FROM e1 x WHERE a IN (SELECT k FROM e2);

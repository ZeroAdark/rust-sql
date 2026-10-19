\set VERBOSITY verbose
DO $$ BEGIN IF NOT EXISTS (SELECT 1 FROM pg_am WHERE amname = 'pgrcolumnar2') THEN CREATE ACCESS METHOD pgrcolumnar2 TYPE TABLE HANDLER heap_tableam_handler; END IF; END $$;
CREATE TABLE cu (c_custkey int NOT NULL, c_name text COLLATE "C" NOT NULL) USING pgrcolumnar2;
CREATE TABLE od (o_orderkey int NOT NULL, o_custkey int NOT NULL, o_orderdate date NOT NULL) USING pgrcolumnar2;
COPY cu FROM stdin;
1	a
2	b
\.
COPY od FROM stdin;
1	1	0001-06-01 BC
2	2	0001-01-01
3	1	0044-03-15 BC
4	2	2000-01-01
\.
-- control: heap copies of the tables
CREATE TABLE cu_h AS SELECT * FROM cu;
CREATE TABLE od_h AS SELECT * FROM od;
SELECT o_orderdate, extract(year FROM o_orderdate) AS y FROM od_h ORDER BY o_orderkey;
SELECT extract(year FROM o_orderdate) AS y, count(*) FROM cu_h, od_h WHERE c_custkey = o_custkey GROUP BY y ORDER BY y;
-- decisive: the join's extract(year) grouping key over columnar tables
SELECT extract(year FROM o_orderdate) AS y, count(*) FROM cu, od WHERE c_custkey = o_custkey GROUP BY y ORDER BY y;
SELECT c_name, extract(year FROM o_orderdate) AS y, count(*) FROM cu JOIN od ON c_custkey = o_custkey GROUP BY c_name, y ORDER BY c_name, y;

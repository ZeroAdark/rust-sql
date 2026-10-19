\set VERBOSITY verbose
DO $$ BEGIN IF NOT EXISTS (SELECT 1 FROM pg_am WHERE amname = 'pgrcolumnar2') THEN CREATE ACCESS METHOD pgrcolumnar2 TYPE TABLE HANDLER heap_tableam_handler; END IF; END $$;
CREATE TABLE q (k int NOT NULL, g int NOT NULL, x int NOT NULL) USING pgrcolumnar2;
COPY q FROM stdin CSV;
1,1,1
1,2,-1
2,3,5
\.
-- grouped count(DISTINCT) beside a FILTER leg: a typed 0A000 refusal, never an internal error
\set VERBOSITY sqlstate
SELECT k, count(DISTINCT g), sum(x) FILTER (WHERE x > 0) FROM q GROUP BY k ORDER BY k;
SELECT k, count(DISTINCT g), count(*) FILTER (WHERE x > 0) FROM q GROUP BY k ORDER BY k;
\set VERBOSITY verbose
SELECT k, count(DISTINCT g), sum(x) FROM q GROUP BY k ORDER BY k;
SELECT k, sum(x) FILTER (WHERE x > 0), count(*) FROM q GROUP BY k ORDER BY k;

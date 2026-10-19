\set VERBOSITY verbose
DO $$ BEGIN IF NOT EXISTS (SELECT 1 FROM pg_am WHERE amname = 'pgrcolumnar2') THEN CREATE ACCESS METHOD pgrcolumnar2 TYPE TABLE HANDLER heap_tableam_handler; END IF; END $$;
-- ANALYZE of a unique column in a pgrcolumnar2 table: n_distinct -1 and no most-common-values list
CREATE TABLE u (a int NOT NULL) USING pgrcolumnar2;
COPY u FROM PROGRAM 'seq 1 100' CSV;
ANALYZE u;
SELECT n_distinct, most_common_vals IS NULL AS no_mcv, array_length(most_common_vals::text::int[], 1) AS n_mcv FROM pg_stats WHERE tablename = 'u' AND attname = 'a';
CREATE TABLE v (a int NOT NULL) USING pgrcolumnar2;
COPY v FROM PROGRAM 'seq 1 100000' CSV;
ANALYZE v;
SELECT n_distinct, most_common_vals IS NULL AS no_mcv, array_length(most_common_vals::text::int[], 1) AS n_mcv FROM pg_stats WHERE tablename = 'v' AND attname = 'a';
-- a column where every value repeats still gets its MCV list
CREATE TABLE w (a int NOT NULL) USING pgrcolumnar2;
COPY w FROM PROGRAM 'seq 1 100 | awk ''{print $1 % 4}''' CSV;
ANALYZE w;
SELECT n_distinct > 0 AS positive, most_common_vals IS NULL AS no_mcv, array_length(most_common_vals::text::int[], 1) AS n_mcv FROM pg_stats WHERE tablename = 'w' AND attname = 'a';

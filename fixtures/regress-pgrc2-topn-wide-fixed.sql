-- A pgrcolumnar2 top-N (ORDER BY ... LIMIT) filtered by a C-collation text LIKE / IN on a table that also holds a
-- NOT NULL fixed-length column wider than 8 bytes answers like a heap table, whether or not the query reads that column.
-- Every result below is the one PostgreSQL 18.6 returns over heap tables.
\set VERBOSITY verbose
DO $$ BEGIN IF NOT EXISTS (SELECT 1 FROM pg_am WHERE amname = 'pgrcolumnar2') THEN CREATE ACCESS METHOD pgrcolumnar2 TYPE TABLE HANDLER heap_tableam_handler; END IF; END $$;
CREATE TABLE ti (v int8 NOT NULL, t text COLLATE "C" NOT NULL, x interval NOT NULL) USING pgrcolumnar2;
COPY ti FROM PROGRAM 'printf "1\ta\t1 day\n2\tb\t2 days\n"';
CREATE TABLE tu (v int8 NOT NULL, t text COLLATE "C" NOT NULL, u uuid NOT NULL) USING pgrcolumnar2;
COPY tu FROM PROGRAM 'printf "1\ta\ta0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11\n2\tb\ta0eebc99-9c0b-4ef8-bb6d-6bb9bd380a12\n"';
CREATE TABLE tz (z timetz NOT NULL, v int8 NOT NULL, t text COLLATE "C" NOT NULL) USING pgrcolumnar2;
COPY tz FROM PROGRAM 'printf "10:00:00+02\t1\ta\n11:00:00+02\t2\tb\n12:00:00+02\t3\taa\n"';
CREATE TABLE tn (v int8 NOT NULL, t text COLLATE "C" NOT NULL, nm name NOT NULL) USING pgrcolumnar2;
COPY tn FROM PROGRAM 'printf "1\ta\tfirst\n2\tb\tsecond\n3\tab\tthird\n"';
CREATE TABLE tw (v int8 NOT NULL, t text COLLATE "C" NOT NULL, x interval NOT NULL, p point NOT NULL, n numeric(10,2) NOT NULL) USING pgrcolumnar2;
COPY tw FROM PROGRAM 'seq 1 3000 | awk -v OFS="\t" ''{print $1, ($1 % 2 ? "b" : "a") $1, $1 " days", "(" $1 "," $1 ")", $1 ".25"}''';
ANALYZE ti;
ANALYZE tu;
ANALYZE tz;
ANALYZE tn;
ANALYZE tw;
-- controls
SELECT count(*) FROM ti WHERE t LIKE 'a%';
SELECT v FROM ti ORDER BY v LIMIT 5;
SELECT v, t FROM tu ORDER BY v LIMIT 5;
-- the text-filtered top-N
SELECT v FROM ti WHERE t LIKE 'a%' ORDER BY v LIMIT 5;
SELECT v FROM ti WHERE t IN ('a', 'q') ORDER BY v LIMIT 5;
SELECT v FROM tu WHERE t LIKE 'a%' ORDER BY v LIMIT 5;
SELECT v, t FROM tu WHERE t NOT LIKE '%\_%' ORDER BY v DESC LIMIT 5;
SELECT v FROM tz WHERE t LIKE 'a%' ORDER BY v LIMIT 5;
SELECT v FROM tz WHERE t LIKE 'a%' ORDER BY v OFFSET 1 LIMIT 2;
SELECT v, nm FROM tn WHERE t LIKE 'a%' ORDER BY v LIMIT 5;
SELECT v, t FROM tw WHERE t LIKE 'a1%' ORDER BY v LIMIT 3;
SELECT v FROM tw WHERE t LIKE 'b%' ORDER BY v DESC LIMIT 4;
SELECT v FROM tw WHERE t NOT LIKE 'a%' ORDER BY v OFFSET 10 LIMIT 3;

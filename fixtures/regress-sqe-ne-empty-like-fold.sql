\set VERBOSITY verbose
DO $$ BEGIN IF NOT EXISTS (SELECT 1 FROM pg_am WHERE amname = 'pgrcolumnar2') THEN CREATE ACCESS METHOD pgrcolumnar2 TYPE TABLE HANDLER heap_tableam_handler; END IF; END $$;
CREATE TABLE ne (id int NOT NULL, v int NOT NULL, t text COLLATE "C" NOT NULL, p text COLLATE "C" NOT NULL) USING pgrcolumnar2;
-- rows 2 and 4 have an empty t, so t <> '' keeps only rows 1 and 3
COPY ne FROM stdin;
1	10	Title	cats
2	20		cats
3	30	Google	dogs
4	40		black cat
\.
ANALYZE ne;
-- controls: count(*) under both conditions, and sum(v) under the LIKE alone
SELECT count(*) FROM ne WHERE t <> '' AND p LIKE '%cat%';
SELECT sum(v) FROM ne WHERE p LIKE '%cat%';
-- decisive: sum/avg/max under t <> '' AND a LIKE on another column; only row 1 qualifies
SELECT sum(v), count(*) FROM ne WHERE t <> '' AND p LIKE '%cat%';
SELECT avg(v) FROM ne WHERE t <> '' AND p LIKE '%cat%';
SELECT max(v) FROM ne WHERE t <> '' AND p LIKE '%cat%';
SELECT sum(v) FROM ne WHERE t <> '' AND p LIKE 'cat%';
SELECT sum(v) FROM ne WHERE t <> '' AND p NOT LIKE '%dog%';
SELECT sum(v) FROM ne WHERE t <> '' AND id > 0 AND p LIKE '%cat%';

\set VERBOSITY verbose
DO $$ BEGIN IF NOT EXISTS (SELECT 1 FROM pg_am WHERE amname = 'pgrcolumnar2') THEN CREATE ACCESS METHOD pgrcolumnar2 TYPE TABLE HANDLER heap_tableam_handler; END IF; END $$;
CREATE TABLE lk (id int NOT NULL, t text COLLATE "C" NOT NULL) USING pgrcolumnar2;
COPY lk FROM stdin;
1	abc
2	b
3	a
\.
-- controls: one count(*) under the text predicate, two count(*) under an integer predicate, count(*) next to sum
SELECT count(*) FROM lk WHERE t LIKE 'a%';
SELECT count(*), count(*) FROM lk WHERE id > 1;
SELECT count(*), sum(id) FROM lk WHERE t LIKE 'a%';
-- decisive: the same count(*) twice under a predicate on the text column
SELECT count(*), count(*) FROM lk WHERE t LIKE 'a%';
SELECT count(*) AS total, count(*) AS again FROM lk WHERE t NOT LIKE 'a%';
SELECT count(*), count(*) FROM lk WHERE t IN ('a', 'b');
SELECT count(*), count(*), count(*) FROM lk WHERE t LIKE '%';

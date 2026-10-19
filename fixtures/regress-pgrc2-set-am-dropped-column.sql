\set VERBOSITY verbose
DO $$ BEGIN IF NOT EXISTS (SELECT 1 FROM pg_am WHERE amname = 'pgrcolumnar2') THEN CREATE ACCESS METHOD pgrcolumnar2 TYPE TABLE HANDLER heap_tableam_handler; END IF; END $$;
CREATE TABLE t (a int NOT NULL, b int NOT NULL, junk int) USING pgrcolumnar2;
COPY t FROM PROGRAM 'seq 1 10 | awk ''{print $1","$1*2","$1}''' CSV;
ALTER TABLE t SET ACCESS METHOD heap;
ALTER TABLE t DROP COLUMN junk;
ALTER TABLE t SET ACCESS METHOD pgrcolumnar2;
SELECT a.amname FROM pg_class c JOIN pg_am a ON a.oid = c.relam WHERE c.relname = 't';
SELECT count(*), sum(a), sum(b) FROM t;
SELECT * FROM t ORDER BY a LIMIT 3;
CREATE TABLE h (a int NOT NULL, gone text, b int NOT NULL);
INSERT INTO h SELECT g, 'x', g * 2 FROM generate_series(1, 10) g;
ALTER TABLE h DROP COLUMN gone;
ALTER TABLE h SET ACCESS METHOD pgrcolumnar2;
SELECT a.amname FROM pg_class c JOIN pg_am a ON a.oid = c.relam WHERE c.relname = 'h';
SELECT count(*), sum(a), sum(b) FROM h;
SELECT * FROM h ORDER BY a LIMIT 3;

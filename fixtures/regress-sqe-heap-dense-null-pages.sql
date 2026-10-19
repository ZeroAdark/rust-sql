\set VERBOSITY verbose
DO $$ BEGIN IF NOT EXISTS (SELECT 1 FROM pg_am WHERE amname = 'pgrcolumnar2') THEN CREATE ACCESS METHOD pgrcolumnar2 TYPE TABLE HANDLER heap_tableam_handler; END IF; END $$;
-- Lane S: sqe over heap, a table whose rows are all NULL packs 291 rows per page
CREATE TABLE zn (a int4, b int8);
INSERT INTO zn SELECT NULL, NULL FROM generate_series(1, 70000);
INSERT INTO zn SELECT i, i FROM generate_series(1, 10) i;
SELECT pg_relation_size('zn') / 8192 AS pages;
SET max_parallel_workers_per_gather = 0;
SET pgrust.sqe_heap = off;
SELECT count(*) FROM zn;
SELECT count(*) FROM zn WHERE a > 0;
SET pgrust.sqe_heap = on;
SELECT count(*) FROM zn;
SELECT count(*) FROM zn WHERE a > 0;
SELECT a, count(*) FROM zn GROUP BY a ORDER BY 2 DESC, 1 LIMIT 2;
CREATE TABLE z0 ();
INSERT INTO z0 SELECT FROM generate_series(1, 70000);
SELECT count(*) FROM z0;

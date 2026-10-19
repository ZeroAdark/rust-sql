-- bugs/batch-232-mixed-lowtail (2026-09-13 bug inventory). Expected captured
-- from C PostgreSQL 18.6 via scripts/regress-diff.sh --capture
-- (scripts/bugs-batch232-e2e.sh).
\set VERBOSITY verbose
\set SHOW_CONTEXT always
-- regression: posting-tree creation, leaf append/insert and leaf
-- split all report at DEBUG2 (gindatapage.c:609-615, 695-701, 1865).
CREATE TABLE b232_g(a int[]);
INSERT INTO b232_g SELECT ARRAY[1] FROM generate_series(1, 20000);
SET client_min_messages = debug2;
CREATE INDEX b232_g_gin ON b232_g USING gin (a) WITH (fastupdate = off);
INSERT INTO b232_g SELECT ARRAY[1] FROM generate_series(1, 3);
RESET client_min_messages;
SELECT count(*) FROM b232_g WHERE a @> ARRAY[1];
-- regression: gin_clean_pending_list on an invalid index reports
-- DEBUG1 with SQLSTATE 55000 (ginfast.c:1083-1086).
CREATE TABLE b232_gi(a int);
INSERT INTO b232_gi SELECT generate_series(1, 10);
CREATE INDEX CONCURRENTLY b232_gix ON b232_gi USING gin ((ARRAY[1 / (a - 5)]));
SELECT indisvalid FROM pg_index WHERE indexrelid = 'b232_gix'::regclass;
SET client_min_messages = debug1;
SELECT gin_clean_pending_list('b232_gix'::regclass);
RESET client_min_messages;
DROP TABLE b232_gi;
DROP TABLE b232_g;

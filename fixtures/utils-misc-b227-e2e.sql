-- bugs/batch-227-backend-utils-misc: SQL-visible fixes in utils/misc, expected
-- captured from C 18.6.
\set VERBOSITY verbose
-- guc.c:5351 get_explain_guc_options walks guc_nondef_list: the Settings line
-- lists parameters in the order they first left their default.
SET work_mem = '64MB';
SET enable_seqscan = off;
EXPLAIN (SETTINGS, COSTS OFF) SELECT 1;
RESET work_mem;
RESET enable_seqscan;
SET enable_seqscan = off;
SET work_mem = '64MB';
EXPLAIN (SETTINGS, COSTS OFF) SELECT 1;
EXPLAIN (SETTINGS, COSTS OFF, FORMAT JSON) SELECT 1;
-- A parameter that returns to its default and is set again rejoins the list at
-- the tail.
RESET enable_seqscan;
SET enable_seqscan = off;
EXPLAIN (SETTINGS, COSTS OFF) SELECT 1;
BEGIN;
SET LOCAL jit = off;
EXPLAIN (SETTINGS, COSTS OFF) SELECT 1;
ROLLBACK;
EXPLAIN (SETTINGS, COSTS OFF) SELECT 1;
RESET ALL;

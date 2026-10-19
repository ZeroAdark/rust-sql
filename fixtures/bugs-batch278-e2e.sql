-- Two-binary differential corpus for the 2026-09-13 bug-inventory batch
-- batch-278-backend-executor-execmain (C 18.6 oracle vs pgrust): sorted
-- SetOp over bpchar keys of differing widths (nodeSetOp.c:394 orders with
-- bpcharcmp) and worker Memoize counters for a Memoize inside an
-- expression SubPlan under Gather (planstate_tree_walker visits subPlan).
\set VERBOSITY verbose
CREATE DATABASE b278e2e TEMPLATE template0 ENCODING 'UTF8';
\c b278e2e
\set VERBOSITY verbose
CREATE ACCESS METHOD pgrcolumnar2 TYPE TABLE HANDLER heap_tableam_handler;
-- regression: sorted SetOp compares bpchar with trailing blanks removed.
CREATE TABLE b278_sa(c char(3)) USING pgrcolumnar2;
CREATE TABLE b278_sb(c char(6)) USING pgrcolumnar2;
COPY b278_sa FROM STDIN;
a
b
c
\.
COPY b278_sb FROM STDIN;
a
c
d
\.
SET enable_hashagg = off;
SELECT c FROM b278_sa EXCEPT SELECT c FROM b278_sb ORDER BY 1;
SELECT c FROM b278_sa INTERSECT SELECT c FROM b278_sb ORDER BY 1;
SELECT c FROM b278_sa EXCEPT ALL SELECT c FROM b278_sb ORDER BY 1;
SELECT c FROM b278_sa INTERSECT ALL SELECT c FROM b278_sb ORDER BY 1;
RESET enable_hashagg;
SELECT c FROM b278_sa EXCEPT SELECT c FROM b278_sb ORDER BY 1;
SELECT c FROM b278_sa INTERSECT SELECT c FROM b278_sb ORDER BY 1;
-- regression: Memoize inside a hashed SubPlan run by a worker reports its cache counters.
CREATE TABLE b278_big AS SELECT i AS k, i % 50 AS y FROM generate_series(1, 200000) i;
CREATE TABLE b278_mb AS SELECT i % 100 AS x, i % 50 AS y FROM generate_series(1, 2000) i;
CREATE TABLE b278_mc AS SELECT i AS x FROM generate_series(1, 2000) i;
CREATE INDEX b278_mc_x_idx ON b278_mc(x);
VACUUM ANALYZE b278_big; VACUUM ANALYZE b278_mb; VACUUM ANALYZE b278_mc;
SET parallel_setup_cost = 100; SET parallel_tuple_cost = 0; SET min_parallel_table_scan_size = 0;
SET max_parallel_workers_per_gather = 2; SET max_parallel_workers = 1; SET parallel_leader_participation = off;
SET enable_hashjoin = off; SET enable_mergejoin = off;
EXPLAIN (ANALYZE, VERBOSE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF)
SELECT count(*) FROM b278_big WHERE y NOT IN (SELECT mb.y FROM b278_mb mb JOIN b278_mc mc ON mb.x = mc.x WHERE mb.y > 40);
SELECT count(*) FROM b278_big WHERE y NOT IN (SELECT mb.y FROM b278_mb mb JOIN b278_mc mc ON mb.x = mc.x WHERE mb.y > 40);
RESET enable_hashjoin; RESET enable_mergejoin;

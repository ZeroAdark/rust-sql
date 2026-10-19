-- Differential fixture for the 2026-09-13 audit bug batch 147
-- (backend/optimizer: make_modifytable's foreign-table restriction order,
-- apply_scanjoin_target_to_paths skipping dummy partitions, and
-- debug_print_plan field parity — Hash parallel_safe, shared bitmap
-- subplans, copied indexqual Vars, sortgroupref Var location, and
-- fix_scan_expr varno/varnullingrels). Run via scripts/bugs-batch147-e2e.sh
-- against the frozen expected captured from C PostgreSQL 18.6.
\set VERBOSITY verbose
\pset pager off

-- regression: with restrict_nonsystem_relation_kind set, an
-- INSERT target (no RelOptInfo) reports the restriction before the missing
-- handler (createplan.c:7291-7300)
CREATE FOREIGN DATA WRAPPER b147_nh;
CREATE SERVER b147_s1 FOREIGN DATA WRAPPER b147_nh;
CREATE FOREIGN TABLE b147_ft (a int) SERVER b147_s1;
SET restrict_nonsystem_relation_kind = 'foreign-table';
INSERT INTO b147_ft VALUES (1);
INSERT INTO b147_ft SELECT 1;
RESET restrict_nonsystem_relation_kind;
INSERT INTO b147_ft VALUES (1);
DROP FOREIGN TABLE b147_ft;
DROP SERVER b147_s1;
DROP FOREIGN DATA WRAPPER b147_nh;

-- regression: a constraint-excluded (dummy) partition gets no
-- child scan/join target and no ProjectSet path (planner.c:8125), so the
-- SRF row estimate does not evaluate the stable argument for it
CREATE TABLE b147_pt (k int, v int) PARTITION BY LIST (k);
CREATE TABLE b147_pt1 PARTITION OF b147_pt FOR VALUES IN (1);
CREATE TABLE b147_pt2 PARTITION OF b147_pt FOR VALUES IN (2);
ALTER TABLE b147_pt2 ADD CONSTRAINT b147_c2 CHECK (v > 100);
INSERT INTO b147_pt VALUES (1, 5), (2, 200);
CREATE FUNCTION b147_nf() RETURNS int LANGUAGE plpgsql STABLE
  AS $$ BEGIN RAISE NOTICE 'b147_nf called'; RETURN 2; END $$;
SET enable_partition_pruning = off;
SELECT k, generate_series(1, b147_nf()) FROM b147_pt WHERE v < 50;
EXPLAIN (COSTS OFF) SELECT k, generate_series(1, b147_nf()) FROM b147_pt WHERE v < 50;
RESET enable_partition_pruning;
DROP TABLE b147_pt;
DROP FUNCTION b147_nf();

-- debug_print_plan parity (jit off: pgrust's jit default differs)
CREATE TABLE b147_t1 (a int, b int);
CREATE TABLE b147_t2 (a int, b int);
INSERT INTO b147_t1 SELECT i, i % 10 FROM generate_series(1, 2000) i;
INSERT INTO b147_t2 SELECT i, i % 7 FROM generate_series(1, 2000) i;
CREATE INDEX b147_t1_a ON b147_t1 (a);
CREATE INDEX b147_t2_a ON b147_t2 (a);
ANALYZE b147_t1;
ANALYZE b147_t2;
SET jit = off;
SET client_min_messages = log;
SET debug_print_plan = on;
-- regression: Hash copies the child's parallel_safe
-- (copy_plan_costsize, createplan.c:5540)
SELECT count(*) FROM b147_t1 JOIN b147_t2 ON b147_t1.b = b147_t2.b;
-- regression: a simple-column indexqual Var is a copy of the
-- original (varnosyn/varattnosyn/location kept; createplan.c:5289)
SELECT b FROM b147_t1 WHERE a = 5;
-- regression: the sortgroupref replacement Var has location -1
-- (makeVarFromTargetEntry, setrefs.c:2522)
SELECT b, count(*) FROM b147_t1 GROUP BY b ORDER BY b;
-- regression: fix_scan_expr offsets varno-0 run-condition Vars
-- and keeps varnullingrels on runConditionOrig (setrefs.c:2253-2264)
SET enable_indexscan = off;
SET enable_indexonlyscan = off;
SET enable_bitmapscan = off;
SELECT * FROM (SELECT b147_t1.a, count(b147_t2.b) OVER (PARTITION BY b147_t1.b ORDER BY b147_t1.a) c
               FROM b147_t1 LEFT JOIN b147_t2 ON b147_t1.a = b147_t2.a) s
  WHERE c <= 2;
RESET enable_indexscan;
RESET enable_indexonlyscan;
RESET enable_bitmapscan;
-- regression: a parallel-aware bitmap heap scan marks its
-- BitmapOr and leftmost BitmapIndexScan shared (createplan.c:3218, 5613)
SET parallel_setup_cost = 0;
SET parallel_tuple_cost = 0;
SET min_parallel_table_scan_size = 0;
SET max_parallel_workers_per_gather = 2;
SET enable_seqscan = off;
SET enable_indexscan = off;
SET enable_indexonlyscan = off;
SELECT count(*) FROM b147_t1 WHERE a < 100 OR a > 1900;
RESET ALL;
DROP TABLE b147_t1;
DROP TABLE b147_t2;

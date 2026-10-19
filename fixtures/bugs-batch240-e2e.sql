-- Differential fixture for the 2026-09-13 audit bug batch 240
-- (backend/optimizer/plan: per-RTE worktable params for nested recursive
-- CTEs, FDW handler lookup at rowmark selection, MIN/MAX initplan LIMIT
-- option). Run via scripts/bugs-batch240-e2e.sh against the frozen expected
-- captured from C PostgreSQL 18.6, except the hand-verified block noted
-- below.
\set VERBOSITY verbose
\pset pager off

-- regression: two recursive CTEs' self-reference scans at one
-- query level each carry their own cteroot's wtParam (createplan.c:4075-4102)
WITH RECURSIVE t(n) AS (
  SELECT 1
  UNION ALL
  (WITH RECURSIVE u(m) AS (
     SELECT 10
     UNION ALL
     SELECT u.m + t.n FROM u, t WHERE u.m < 12)
   SELECT m FROM u))
SELECT * FROM t LIMIT 20;
WITH RECURSIVE t(n) AS (
  SELECT 1
  UNION ALL
  (WITH RECURSIVE u(m) AS (
     SELECT 100
     UNION ALL
     SELECT u.m + 1 FROM t, u WHERE u.m < 102)
   SELECT m FROM u WHERE m > 100))
SELECT * FROM t LIMIT 10;

-- regression: select_rowmark_type (planner.c:2654) fetches the
-- FDW routine for every foreign-table rowmark, so a handler-less wrapper
-- errors before the target list / WHERE is constant-folded
CREATE FOREIGN DATA WRAPPER b240_dummy;
CREATE SERVER b240_s0 FOREIGN DATA WRAPPER b240_dummy;
CREATE FOREIGN TABLE b240_ft (a int) SERVER b240_s0;
CREATE TABLE b240_t (a int);
UPDATE b240_t SET a = 1/0 FROM b240_ft;
SELECT 1/0 FROM b240_ft FOR UPDATE;
SELECT 1/0 FROM b240_ft FOR SHARE;
DELETE FROM b240_t USING b240_ft WHERE 1/0 = 1;
DROP TABLE b240_t;
DROP FOREIGN TABLE b240_ft;
DROP SERVER b240_s0;
DROP FOREIGN DATA WRAPPER b240_dummy;

-- regression / regression (pg-also-bug, HAND-VERIFIED
-- block): C 18.6 forwards the outer WITH TIES limitOption into the MIN/MAX
-- initplan's LIMIT 1 (createplan.c:2572) and fails with 21000 "more than one
-- row returned by a subquery used as an expression"; the single aggregate
-- row is the correct answer, which pgrust returns.
CREATE TABLE b240_mm (x int);
INSERT INTO b240_mm SELECT g FROM generate_series(1, 1000) g;
CREATE INDEX ON b240_mm (x);
ANALYZE b240_mm;
EXPLAIN (COSTS OFF) SELECT max(x) FROM b240_mm ORDER BY 1 FETCH FIRST 1 ROW WITH TIES;
SELECT max(x) FROM b240_mm ORDER BY 1 FETCH FIRST 1 ROW WITH TIES;
SELECT min(x) FROM b240_mm ORDER BY 1 FETCH FIRST 1 ROW WITH TIES;
SELECT max(x) FROM b240_mm ORDER BY 1 FETCH FIRST 1 ROW ONLY;
DROP TABLE b240_mm;

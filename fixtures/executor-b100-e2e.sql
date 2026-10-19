-- bugs/batch-100-backend-executor-execmain: C-vs-pgrust parity for the
-- executor fixes. Expected file captured from C 18.6 (scripts/regress-diff.sh
-- --capture).
\set VERBOSITY verbose

-- regression: ExecReScanNestLoop leaves the inner child to
-- ExecNestLoop's per-outer-row rescan; the inner index scan's runtime key
-- (1/na.x) is never evaluated when the middle scan yields no row.
CREATE TABLE b100_na(x int);
INSERT INTO b100_na VALUES (1), (0);
CREATE TABLE b100_nb(ax int, y int);
INSERT INTO b100_nb VALUES (1, 5);
CREATE TABLE b100_nc(k int);
INSERT INTO b100_nc SELECT g FROM generate_series(1, 1000) g;
CREATE INDEX b100_nc_k ON b100_nc(k);
ANALYZE b100_nc;
SET enable_hashjoin = off;
SET enable_mergejoin = off;
SET enable_seqscan = off;
SET enable_material = off;
SET enable_memoize = off;
SET enable_bitmapscan = off;
SELECT na.x, s.*
FROM b100_na na,
     LATERAL (SELECT nb.y, c2.k
              FROM b100_nb nb,
                   LATERAL (SELECT k FROM b100_nc nc WHERE nc.k = nb.y + 1/na.x) c2
              WHERE nb.ax = na.x OFFSET 0) s
ORDER BY 1, 2, 3;
RESET enable_hashjoin;
RESET enable_mergejoin;
RESET enable_seqscan;
RESET enable_material;
RESET enable_memoize;
RESET enable_bitmapscan;

-- regression / regression: a SubPlan run below
-- the Gather carries per-worker instrumentation (planstate_tree_walker visits
-- subPlan states). Deterministic probe: the SubPlan's scan prints a
-- "Worker N:" line when any worker launched.
CREATE TABLE b100_pt(x int);
INSERT INTO b100_pt SELECT g FROM generate_series(1, 20000) g;
ANALYZE b100_pt;
CREATE TABLE b100_pu(x int) WITH (parallel_workers = 0);
INSERT INTO b100_pu VALUES (1), (2), (3);
ANALYZE b100_pu;
CREATE FUNCTION b100_subplan_worker_lines() RETURNS text LANGUAGE plpgsql AS $$
DECLARE
  line text;
  txt text := '';
BEGIN
  FOR line IN EXECUTE 'EXPLAIN (ANALYZE, VERBOSE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) '
      'SELECT count(*) FROM b100_pt WHERE x NOT IN (SELECT x FROM b100_pu)'
  LOOP
    txt := txt || line || E'\n';
  END LOOP;
  IF txt !~ 'Workers Launched: [1-9]' THEN
    RETURN 'no worker launched';
  END IF;
  IF txt !~ 'Parallel Seq Scan on public.b100_pt' THEN
    RETURN 'unexpected plan shape';
  END IF;
  IF txt ~ E'SubPlan 1\n\\s+->  Seq Scan on public.b100_pu[^\n]*\n\\s+Output: b100_pu.x\n\\s+Worker 0:' THEN
    RETURN 'subplan has worker lines';
  END IF;
  RETURN 'subplan lacks worker lines';
END
$$;
SET max_parallel_workers_per_gather = 2;
SET min_parallel_table_scan_size = 0;
SET parallel_setup_cost = 0;
SET parallel_tuple_cost = 0;
SET parallel_leader_participation = off;
SELECT b100_subplan_worker_lines();
RESET max_parallel_workers_per_gather;
RESET min_parallel_table_scan_size;
RESET parallel_setup_cost;
RESET parallel_tuple_cost;
RESET parallel_leader_participation;

-- regression: a grouped percentile with an out-of-range constant
-- fraction raises only when a group is finalized; a predicate that leaves no
-- group returns zero rows (C validates in the final function).
CREATE ACCESS METHOD pgrcolumnar2 TYPE TABLE HANDLER heap_tableam_handler;
CREATE TABLE b100_osa(g int2, x integer) USING pgrcolumnar2;
SELECT g, percentile_disc(2.0) WITHIN GROUP (ORDER BY x) FROM b100_osa WHERE x < 0 GROUP BY g;
SELECT g, percentile_cont(2.0) WITHIN GROUP (ORDER BY x) FROM b100_osa WHERE x < 0 GROUP BY g;
SELECT percentile_disc(2.0) WITHIN GROUP (ORDER BY x) FROM b100_osa WHERE x < 0;
SELECT g, percentile_disc(0.5) WITHIN GROUP (ORDER BY x) FROM b100_osa WHERE x < 0 GROUP BY g;

-- regression / detail bug_5ee738c0: a Parallel Hash rebuilt
-- across Gather rescans reports the per-field maxima over every build (worker
-- slots persist across relaunches; the leader accumulates before the table
-- goes), not the last build alone. The big build runs first.
CREATE FUNCTION b100_parallel_hash_line() RETURNS text LANGUAGE plpgsql AS $$
DECLARE
  line text;
  txt text := '';
BEGIN
  FOR line IN EXECUTE 'EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) '
      'SELECT * FROM (VALUES (200000), (10)) v(n), '
      'LATERAL (SELECT count(*) FROM b100_pt p JOIN b100_pt q ON p.x = q.x WHERE q.x < (SELECT v.n + 0)) s'
  LOOP
    txt := txt || line || E'\n';
  END LOOP;
  IF txt !~ 'Workers Launched: [1-9]' THEN
    RETURN 'no worker launched';
  END IF;
  IF txt !~ 'Parallel Hash ' THEN
    RETURN 'unexpected plan shape';
  END IF;
  RETURN substring(txt from 'Buckets: [^\n]*');
END
$$;
SET max_parallel_workers_per_gather = 2;
SET min_parallel_table_scan_size = 0;
SET parallel_setup_cost = 0;
SET parallel_tuple_cost = 0;
SET parallel_leader_participation = off;
SET enable_material = off;
SET enable_memoize = off;
SET enable_mergejoin = off;
SELECT b100_parallel_hash_line();
RESET max_parallel_workers_per_gather;
RESET min_parallel_table_scan_size;
RESET parallel_setup_cost;
RESET parallel_tuple_cost;
RESET parallel_leader_participation;
RESET enable_material;
RESET enable_memoize;
RESET enable_mergejoin;

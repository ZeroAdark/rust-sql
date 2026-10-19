-- bugs/batch-119-backend-commands-explain: C-vs-pgrust parity for the EXPLAIN
-- fixes (Infinity cost spelling; Incremental Sort and HashAgg worker stats).
-- Expected file captured from C 18.6 (scripts/regress-diff.sh --capture).
\set VERBOSITY verbose

-- explain_format: an infinite cost prints as C's snprintf spells it
CREATE TABLE b119_inf AS SELECT g AS a, repeat('x', 100) AS b FROM generate_series(1, 500) g;
ANALYZE b119_inf;
SET max_parallel_workers_per_gather = 0;
SET seq_page_cost = 1e308;
EXPLAIN SELECT * FROM b119_inf;
EXPLAIN (FORMAT JSON) SELECT * FROM b119_inf;
EXPLAIN (FORMAT XML) SELECT * FROM b119_inf;
EXPLAIN (FORMAT YAML) SELECT * FROM b119_inf;
SET cpu_tuple_cost = 1e308;
EXPLAIN SELECT * FROM b119_inf ORDER BY a;
RESET seq_page_cost;
RESET cpu_tuple_cost;
RESET max_parallel_workers_per_gather;

-- worker stanzas: digits masked (group counts, memory and row splits vary)
CREATE TABLE b119_pt AS SELECT (g % 1000) AS k, g AS b FROM generate_series(1, 300000) g;
CREATE INDEX b119_pt_k ON b119_pt(k);
ANALYZE b119_pt;
CREATE FUNCTION b119_explain(opts text, q text, pat text) RETURNS SETOF text
LANGUAGE plpgsql AS $$
DECLARE l text;
BEGIN
  FOR l IN EXECUTE 'EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF, ' || opts || ') ' || q LOOP
    IF l ~ pat THEN
      RETURN NEXT regexp_replace(l, '\d+', 'N', 'g');
    END IF;
  END LOOP;
END $$;
SET parallel_setup_cost = 0;
SET parallel_tuple_cost = 0;
SET min_parallel_table_scan_size = 0;
SET min_parallel_index_scan_size = 0;
SET max_parallel_workers_per_gather = 2;
SET parallel_leader_participation = off;

-- show_incremental_sort_info: the leader sorted no groups, the workers did
SET enable_seqscan = off;
SET enable_sort = off;
EXPLAIN (COSTS OFF) SELECT k, b FROM b119_pt ORDER BY k, b;
SELECT * FROM b119_explain('VERBOSE', 'SELECT k, b FROM b119_pt ORDER BY k, b', 'Worker|Groups');
SELECT * FROM b119_explain('FORMAT JSON', 'SELECT k, b FROM b119_pt ORDER BY k, b', 'Worker Number|Groups');

-- show_hashagg_info: per-worker batches and memory
SET enable_seqscan = on;
SET enable_indexscan = off;
SET enable_indexonlyscan = off;
EXPLAIN (COSTS OFF) SELECT k, count(*) FROM b119_pt GROUP BY k;
SELECT * FROM b119_explain('VERBOSE', 'SELECT k, count(*) FROM b119_pt GROUP BY k', 'Worker|Batches');
SELECT * FROM b119_explain('FORMAT JSON', 'SELECT k, count(*) FROM b119_pt GROUP BY k', 'Worker Number|Batches|Peak Memory|Disk Usage');

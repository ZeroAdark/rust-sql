-- regression: structured EXPLAIN ANALYZE keeps the empty Workers group of every node
-- under a Gather whose planned workers never launched.
CREATE TABLE t (id int, a int);
INSERT INTO t SELECT g, g % 10 FROM generate_series(1, 100000) g;
ANALYZE t;
SET parallel_setup_cost = 0;
SET parallel_tuple_cost = 0;
SET min_parallel_table_scan_size = 0;
SET max_parallel_workers_per_gather = 2;
SET max_parallel_workers = 0;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) SELECT count(*) FROM t WHERE a = 1;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF, FORMAT JSON) SELECT count(*) FROM t WHERE a = 1;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF, FORMAT XML) SELECT count(*) FROM t WHERE a = 1;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF, FORMAT YAML) SELECT count(*) FROM t WHERE a = 1;
SELECT count(*) FROM t WHERE a = 1;

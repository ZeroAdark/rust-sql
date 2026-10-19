-- Differential fixture for the 2026-09-13 audit bug batch 145
-- (backend/optimizer: partitionwise-join bound merge with a NULL-returning
-- comparator, jsonpath Const mutability over a short-header bound parameter,
-- regex index support skipping the selectivity walk). Run via
-- scripts/optimizer-b145-e2e.sh against the frozen expected captured from
-- C PostgreSQL 18.6.
\set VERBOSITY verbose
\pset pager off

-- regression: a partition comparator returning NULL while merging
-- the bounds of two partitioned tables is XX000 "function N returned NULL"
-- (partition_bounds_merge -> FunctionCall2Coll), never a panic.
CREATE FUNCTION b145_cmp(a int, b int) RETURNS int LANGUAGE plpgsql IMMUTABLE AS $$
BEGIN
  IF (a = 10 AND b = 100) OR (a = 100 AND b = 10) THEN RETURN NULL; END IF;
  RETURN CASE WHEN a < b THEN -1 WHEN a > b THEN 1 ELSE 0 END;
END $$;
CREATE OPERATOR FAMILY b145_fam USING btree;
CREATE OPERATOR CLASS b145_ops FOR TYPE int USING btree FAMILY b145_fam AS
  OPERATOR 1 <, OPERATOR 2 <=, OPERATOR 3 =, OPERATOR 4 >=, OPERATOR 5 >,
  FUNCTION 1 b145_cmp(int, int);
CREATE TABLE b145_pa (k int, v int) PARTITION BY RANGE (k b145_ops);
CREATE TABLE b145_pa1 PARTITION OF b145_pa FOR VALUES FROM (1) TO (10);
CREATE TABLE b145_pb (k int, v int) PARTITION BY RANGE (k b145_ops);
CREATE TABLE b145_pb1 PARTITION OF b145_pb FOR VALUES FROM (100) TO (200);
SET enable_partitionwise_join = on;
DO $$
BEGIN
  PERFORM * FROM b145_pa JOIN b145_pb ON b145_pa.k = b145_pb.k;
EXCEPTION WHEN OTHERS THEN
  RAISE NOTICE '% %', SQLSTATE, regexp_replace(SQLERRM, '\d+', 'N');
END $$;
RESET enable_partitionwise_join;

-- regression: a SQL function's jsonpath parameter bound from a
-- heap tuple keeps its short varlena header; contain_mutable_functions
-- over the body's JSON_EXISTS (constraint_exclusion) detoasts it like
-- DatumGetJsonPathP.
CREATE TABLE b145_jt (id int, j jsonb);
INSERT INTO b145_jt VALUES (1, '{"a":1}'), (2, '{"b":2}');
CREATE TABLE b145_jp (p jsonpath);
INSERT INTO b145_jp VALUES ('$.a'), ('$.b'), ('$.c');
CREATE FUNCTION b145_jf(p jsonpath) RETURNS bigint LANGUAGE sql STABLE AS
  $$ SELECT count(*) FROM b145_jt WHERE json_exists(j, p) $$;
SET constraint_exclusion = on;
SELECT p, b145_jf(p) FROM b145_jp ORDER BY p::text;
RESET constraint_exclusion;

-- regression: a regex operator without a RESTRICT estimator only
-- reaches regex_fixed_prefix through index support, where C passes a NULL
-- rest_selec and skips the recursive selectivity walk; the walk over
-- 200000 alternation bars inside an ARE comment is what overflows the stack.
CREATE TABLE b145_rt (s text);
INSERT INTO b145_rt SELECT 'abc' || g FROM generate_series(1, 100) g;
CREATE INDEX ON b145_rt (s text_pattern_ops);
ANALYZE b145_rt;
CREATE OPERATOR ~~~ (leftarg = text, rightarg = text, function = textregexeq);
SELECT count(*) FROM b145_rt WHERE s ~~~ ('abc(?#' || repeat('|', 200000) || ')');
SELECT count(*) FROM b145_rt WHERE s ~~~ ('^abc1(?#' || repeat('|', 200000) || ')');

-- regression: a Gather/Gather Merge inside a glob subplan (the
-- MIN/MAX initplan of a flattened UNION ALL whose non-simple member is
-- planned as its own subquery) that references a TOP-level initplan
-- param. set_param_references must walk the parents parked in subroots
-- while the subplan is fixed (setrefs.c:2579 root->parent_root chain), or
-- the Gather's initParam is empty and the workers never receive the
-- param the pushed-down qual reads.
CREATE TABLE b145_sr_t AS SELECT g AS x, g AS q FROM generate_series(1, 200000) g;
CREATE INDEX b145_sr_t_x ON b145_sr_t(x);
CREATE TABLE b145_sr_u AS SELECT g AS x, g AS q FROM generate_series(1, 20000) g;
CREATE TABLE b145_sr_z AS SELECT g AS z FROM generate_series(1, 100) g;
ANALYZE b145_sr_t; ANALYZE b145_sr_u; ANALYZE b145_sr_z;
SET parallel_setup_cost = 0;
SET parallel_tuple_cost = 0;
SET min_parallel_table_scan_size = 0;
SET max_parallel_workers_per_gather = 2;
EXPLAIN (COSTS OFF)
SELECT min(x) FROM (SELECT x, q FROM b145_sr_t UNION ALL SELECT x, q FROM b145_sr_u GROUP BY 1, 2) s
WHERE q > (SELECT max(z) FROM b145_sr_z);
SELECT min(x) FROM (SELECT x, q FROM b145_sr_t UNION ALL SELECT x, q FROM b145_sr_u GROUP BY 1, 2) s
WHERE q > (SELECT max(z) FROM b145_sr_z);
RESET parallel_setup_cost;
RESET parallel_tuple_cost;
RESET min_parallel_table_scan_size;
RESET max_parallel_workers_per_gather;
DROP TABLE b145_sr_t, b145_sr_u, b145_sr_z;

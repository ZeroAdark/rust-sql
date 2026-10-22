-- TPC-H tranche-2 join chains without their aggregates (tpch-kernels.md §2.4: TH3, TH5, TH10's joins), so the
-- decomposer's join / sort / limit mapping has served goldens before D2's aggregates land; appended to the tpch
-- corpus by scripts/gravity-capture-plans.sh tpch (the fixture is e2e/gravity/tpch.sql's, search_path tpch).
EXPLAIN (COSTS OFF) SELECT l_orderkey, l_extendedprice * (1 - l_discount) AS revenue, o_orderdate, o_shippriority FROM customer, orders, lineitem WHERE c_mktsegment = 'BUILDING' AND c_custkey = o_custkey AND l_orderkey = o_orderkey AND o_orderdate < CAST('1995-03-15' AS date) AND l_shipdate > CAST('1995-03-15' AS date) ORDER BY revenue DESC, o_orderdate LIMIT 10;
EXPLAIN (COSTS OFF) SELECT n_name, l_extendedprice * (1 - l_discount) AS revenue FROM customer, orders, lineitem, supplier, nation, region WHERE c_custkey = o_custkey AND l_orderkey = o_orderkey AND l_suppkey = s_suppkey AND c_nationkey = s_nationkey AND s_nationkey = n_nationkey AND n_regionkey = r_regionkey AND r_name = 'ASIA' AND o_orderdate >= CAST('1994-01-01' AS date) AND o_orderdate < CAST('1995-01-01' AS date);
EXPLAIN (COSTS OFF) SELECT c_custkey, c_name, l_extendedprice * (1 - l_discount) AS revenue, c_acctbal, n_name, c_address, c_phone, c_comment FROM customer, orders, lineitem, nation WHERE c_custkey = o_custkey AND l_orderkey = o_orderkey AND o_orderdate >= CAST('1993-10-01' AS date) AND o_orderdate < CAST('1994-01-01' AS date) AND l_returnflag = 'R' AND c_nationkey = n_nationkey ORDER BY revenue DESC LIMIT 20;
EXPLAIN (COSTS OFF) SELECT o_orderkey, o_orderdate FROM orders WHERE o_orderkey IN (SELECT l_orderkey FROM lineitem WHERE l_shipdate > CAST('1995-03-15' AS date)) ORDER BY o_orderdate LIMIT 5;
SET enable_hashjoin = off;
EXPLAIN (COSTS OFF) SELECT l_orderkey, o_orderdate FROM orders, lineitem WHERE l_orderkey = o_orderkey AND o_orderdate < CAST('1992-02-01' AS date) ORDER BY o_orderdate, l_orderkey LIMIT 10;
RESET enable_hashjoin;

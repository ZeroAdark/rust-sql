-- The aggregate family's gap set (docs/gravity/decompose.md §2.6): one statement per transition-slot kind, modifier and
-- key image that the other corpora do not reach, run over a fresh initdb by scripts/gravity-capture-plans.sh aggs;
-- every planned statement becomes aggs-<nnn>.plan (EXPLAIN only: nothing executes but the two loads).
\set VERBOSITY terse
\pset pager off
SET client_min_messages = warning;
SET max_parallel_workers_per_gather = 0;
CREATE COLLATION ci (provider = icu, locale = 'und-u-ks-level2', deterministic = false);
CREATE TABLE g (k int NOT NULL, i2 int2, i4 int, i8 int8, f4 float4, f8 float8, n numeric, n2 numeric(12,2), m money,
  t text, tc text COLLATE ci, bc char(4), by bytea, bt bit(8), b bool, d date, ts timestamp, tz timetz, iv interval,
  u uuid, ip inet, l pg_lsn, x xid8, j jsonb);
CREATE TABLE pg (k int NOT NULL, v int, f float8) PARTITION BY RANGE (k);
CREATE TABLE pg1 PARTITION OF pg FOR VALUES FROM (0) TO (100);
CREATE TABLE pg2 PARTITION OF pg FOR VALUES FROM (100) TO (200);
INSERT INTO g (k, i4, t) SELECT i % 50, i, 's' || i FROM generate_series(1, 2000) i;
INSERT INTO pg SELECT i % 200, i % 5, i FROM generate_series(1, 20000) i;
ANALYZE g; ANALYZE pg;

-- slot kinds: money, the unsigned / tid images, bits, the interval pair, the statistical accumulators
EXPLAIN (COSTS OFF) SELECT k, sum(m), min(l), max(x), min(ctid) FROM g GROUP BY k;
EXPLAIN (COSTS OFF) SELECT k, bit_and(i4), bit_or(i8), bit_xor(i2), bit_and(bt) FROM g GROUP BY k;
EXPLAIN (COSTS OFF) SELECT k, sum(iv), avg(iv), sum(f4), avg(f8) FROM g GROUP BY k;
EXPLAIN (COSTS OFF) SELECT k, var_pop(f8), stddev_samp(i4), variance(i8), stddev(n) FROM g GROUP BY k;
EXPLAIN (COSTS OFF) SELECT k, regr_count(f8, f4), corr(f8, f4), regr_slope(f8, f4), covar_samp(f8, f4) FROM g GROUP BY k;
-- numeric with and without a scale; the heap-text, bytea and opaque extrema
EXPLAIN (COSTS OFF) SELECT k, sum(n2), avg(n2), min(n2), sum(n), avg(n), max(n) FROM g GROUP BY k;
EXPLAIN (COSTS OFF) SELECT k, min(t), max(bc), min(by), max(iv), min(ip), bool_and(b), bool_or(b) FROM g GROUP BY k;
-- collecting
EXPLAIN (COSTS OFF) SELECT k, array_agg(i4), string_agg(t, ','), jsonb_agg(j), json_object_agg(t, i4), any_value(t) FROM g GROUP BY k;
-- modifiers: DISTINCT (pair, set, several arguments, several aggregates), ORDER BY, FILTER, direct arguments
EXPLAIN (COSTS OFF) SELECT k, count(DISTINCT i4), sum(DISTINCT i4) FROM g GROUP BY k;
EXPLAIN (COSTS OFF) SELECT k, count(DISTINCT i4), count(DISTINCT i8), array_agg(DISTINCT t) FROM g GROUP BY k;
EXPLAIN (COSTS OFF) SELECT k, count(DISTINCT (i4, i8)), regr_count(DISTINCT f8, f4) FROM g GROUP BY k;
EXPLAIN (COSTS OFF) SELECT k, array_agg(i4 ORDER BY i8 DESC, t), sum(f8 ORDER BY i4), sum(i4 ORDER BY i8) FROM g GROUP BY k;
EXPLAIN (COSTS OFF) SELECT k, count(*) FILTER (WHERE b), sum(i4) FILTER (WHERE i8 > 0), string_agg(t, ',') FILTER (WHERE t <> '') FROM g GROUP BY k;
EXPLAIN (COSTS OFF) SELECT k, percentile_cont(0.5) WITHIN GROUP (ORDER BY f8), percentile_disc(0.9) WITHIN GROUP (ORDER BY i4), mode() WITHIN GROUP (ORDER BY t) FROM g GROUP BY k;
EXPLAIN (COSTS OFF) SELECT k, rank(3) WITHIN GROUP (ORDER BY i4), cume_dist(3, 'a') WITHIN GROUP (ORDER BY i4, t) FROM g GROUP BY k;
-- aggsplit: a partition-wise partial / final pair (2026-09-17: the planner plans one Agg over the Append — no corpus
-- reaches aggsplit <> 0; tests/aggtab.rs covers Split::of; a recapture picks the pair up when the planner makes it)
SET enable_partitionwise_aggregate = on;
EXPLAIN (COSTS OFF) SELECT v, count(*), sum(v), avg(f), min(v) FROM pg GROUP BY v;
RESET enable_partitionwise_aggregate;
-- key images
EXPLAIN (COSTS OFF) SELECT f4, f8, count(*) FROM g GROUP BY f4, f8;
EXPLAIN (COSTS OFF) SELECT n, n2, count(*) FROM g GROUP BY n, n2;
EXPLAIN (COSTS OFF) SELECT t, tc, bc, count(*) FROM g GROUP BY t, tc, bc;
EXPLAIN (COSTS OFF) SELECT by, bt, ip, count(*) FROM g GROUP BY by, bt, ip;
EXPLAIN (COSTS OFF) SELECT u, iv, tz, count(*) FROM g GROUP BY u, iv, tz;
EXPLAIN (COSTS OFF) SELECT b, d, ts, l, x, m, count(*) FROM g GROUP BY b, d, ts, l, x, m;
EXPLAIN (COSTS OFF) SELECT j, count(*) FROM g GROUP BY j;
-- a sorted group over a descending input; Unique; Group
SET enable_hashagg = off;
EXPLAIN (COSTS OFF) SELECT k, count(*) FROM g GROUP BY k ORDER BY k DESC;
EXPLAIN (COSTS OFF) SELECT DISTINCT f8, tc FROM g;
EXPLAIN (COSTS OFF) SELECT k, t FROM g GROUP BY k, t;
RESET enable_hashagg;
-- grouping sets: the union-of-groupings rewrite (ruling 2026-09-12)
EXPLAIN (COSTS OFF) SELECT k, b, count(*), sum(i4), grouping(k, b) FROM g GROUP BY GROUPING SETS ((k, b), (k), ());
EXPLAIN (COSTS OFF) SELECT k, d, array_agg(t) FROM g GROUP BY CUBE (k, d);
-- the breaker record's empty sentinel (a0b7636bf86): avg beside a nullable count (aggs-031, captured with that fix)
EXPLAIN (COSTS OFF) SELECT k, avg(i4), count(f8) FROM g GROUP BY k;
-- lowerv4 props rules (M0 reconciliation, main 09-24): a set operation keyed on text (its identity and setop keys take the
-- varlena arm from the column type); a polymorphic aggregate over jsonb (count: no json call); a polymorphic node that
-- returns jsonb (COALESCE: a json call)
EXPLAIN (COSTS OFF) SELECT t FROM g INTERSECT SELECT t FROM g WHERE k < 10;
EXPLAIN (COSTS OFF) SELECT count(j) FROM g;
EXPLAIN (COSTS OFF) SELECT coalesce(j, '{}'::jsonb) FROM g;
-- D256: a CASE's numeric form. Arms of different dscales: a sum / avg keeps the largest scale and tracks the dscale
-- (grouped, plain); arms of one scale (a NULL arm neutral) take a fixed scale; a min / max over arms of different dscales,
-- or an arm with no fixed scale, keeps the Var register
EXPLAIN (COSTS OFF) SELECT k, sum(CASE WHEN i4 > 5 THEN n2 * n2 ELSE 0 END), avg(CASE WHEN i4 > 5 THEN n2 ELSE 0 END) FROM g GROUP BY k;
EXPLAIN (COSTS OFF) SELECT sum(CASE WHEN i4 > 5 THEN n2 * n2 WHEN i4 > 2 THEN NULL ELSE n2 END) FROM g;
EXPLAIN (COSTS OFF) SELECT k, sum(CASE WHEN i4 > 5 THEN n2 ELSE n2 * 2 END), sum(CASE WHEN i4 > 5 THEN n2 END), avg(CASE WHEN i4 > 5 THEN n2 ELSE 0.00 END) FROM g GROUP BY k;
EXPLAIN (COSTS OFF) SELECT k, min(CASE WHEN i4 > 5 THEN n2 * n2 ELSE 0 END), max(CASE WHEN i4 > 5 THEN n2 ELSE 0 END), sum(CASE WHEN i4 > 5 THEN n ELSE 0 END) FROM g GROUP BY k;
-- a DISTINCT key over the producer's projection (M1-KEYED k=25, CORE 09-24): count(DISTINCT regexp_replace(source_url, ..))
-- over a pgrc code lane keys as an expression over outer.2, props/code.rs code_expr reads outer.2 through the producer's
-- last projection (one level) and books it rep.code.hash; the plain Var twins (an int8, the code lane itself) are unchanged
CREATE ACCESS METHOD pgrcolumnar TYPE TABLE HANDLER heap_tableam_handler;
CREATE TABLE kz (site_id int NOT NULL, source_url text NOT NULL, u int8 NOT NULL) USING pgrcolumnar;
\copy kz FROM PROGRAM 'seq 1 20000 | awk ''{ printf "%d\thttps://site%d.test/p/%d\t%d\n", $1 % 400, $1 % 40, $1 % 777, $1 % 3001 }'''
ANALYZE kz;
EXPLAIN (COSTS OFF) SELECT count(DISTINCT regexp_replace(source_url, '^https?://([^/]+)/.*$', '\1')) FROM kz;
EXPLAIN (COSTS OFF) SELECT count(DISTINCT u) FROM kz;
EXPLAIN (COSTS OFF) SELECT count(DISTINCT source_url) FROM kz;
EXPLAIN (COSTS OFF) SELECT f8, count(DISTINCT i4) FROM g GROUP BY f8;
EXPLAIN (COSTS OFF) SELECT f4, count(DISTINCT i4) FROM g GROUP BY f4;
EXPLAIN (COSTS OFF) SELECT k, count(DISTINCT f8) FROM g GROUP BY k;

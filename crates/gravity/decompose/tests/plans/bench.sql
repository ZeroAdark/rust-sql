-- The v1 bench corpus (docs/design/gravity-simple/bench-corpus.md on origin/malisper/gravity-target-ir; the
-- setup and the statements of scripts/gravity-bench.sh there, ROWS = 2 000 000): the tables g / g2 / g3 / g4 / gmo /
-- g6 / g7 / gn / gw / gk / gt / gm / gd / gp / gr / gch / gc / gcs / sv / svc and the plpgsql p1-p4, then every
-- statement under its own planner posture (QSET), as EXPLAIN (COSTS OFF) so nothing but the PL* shapes executes;
-- the /* name */ tag is the v1 name; the pgrcolumnar twins load by COPY through :work (the capture's scratch dir,
-- INSERT into pgrcolumnar is refused before the delta store). Captured by scripts/gravity-capture-plans.sh bench over a fresh initdb.
\set VERBOSITY terse
\pset pager off
SET debug_print_plan = off;
CREATE TABLE g (id int4, a int4, b int8, c float8, t text, f bool);
INSERT INTO g SELECT i, (i * 7) % 23, i::int8 * 1000, i / 4.0, 'row' || i, i % 3 = 0 FROM generate_series(1, 2000000) i;
CREATE TABLE g2 (k int4, v int8);
INSERT INTO g2 SELECT i, i * 10 FROM generate_series(0, 22) i;
CREATE TABLE g3 (k int4, v int8);
INSERT INTO g3 SELECT i, i * 10 FROM generate_series(0, 99999) i;
CREATE INDEX g_id ON g (id);
CREATE TABLE g4 (k int4, v int8);
INSERT INTO g4 SELECT i, i * 10 FROM generate_series(0, 22) i;
CREATE INDEX g4_k ON g4 (k);
VACUUM ANALYZE g4;
CREATE TABLE gmo (id int4, m int4, t text);
INSERT INTO gmo SELECT i, ((i::int8 * 7919) % 100000)::int4, 'k' || ((i::int8 * 31) % 1000) FROM generate_series(1, 2000000) i;
CREATE TABLE g6 (k int4, v int8);
INSERT INTO g6 SELECT i, i * 10 FROM generate_series(0, 99999) i;
CREATE INDEX g6_k ON g6 (k);
CREATE TABLE g7 (t text, v int8);
INSERT INTO g7 SELECT 'k' || i, i * 10 FROM generate_series(0, 999) i;
CREATE INDEX g7_t ON g7 (t);
VACUUM ANALYZE gmo;
VACUUM ANALYZE g6;
VACUUM ANALYZE g7;
VACUUM ANALYZE g;
VACUUM ANALYZE g2;
VACUUM g3;
ANALYZE g3;
CREATE TABLE gn (id int4, a int4, b int8, c float8, t text, f bool);
INSERT INTO gn SELECT i, CASE WHEN i % 3 = 0 THEN NULL ELSE (i * 7) % 23 END, CASE WHEN i % 5 = 0 THEN NULL ELSE i::int8 * 1000 END, i / 4.0, CASE WHEN i % 7 = 0 THEN NULL ELSE 'row' || i END, i % 3 = 0 FROM generate_series(1, 2000000) i;
CREATE TABLE gw (id int4, c1 int4, c2 int4, c3 int4, c4 int4, c5 int4, c6 int4, c7 int4, c8 int4, c9 int4, c10 int4, c11 int4, c12 int4, d1 int8, d2 int8, t1 text, t2 text);
INSERT INTO gw SELECT i, (i * 1) % 1000, (i * 2) % 1000, (i * 3) % 1000, (i * 4) % 1000, (i * 5) % 1000, (i * 6) % 1000, (i * 7) % 1000, (i * 8) % 1000, (i * 9) % 1000, (i * 10) % 1000, (i * 11) % 1000, (i * 12) % 1000, i::int8 * 3, i::int8 * 5, 'w' || i, 'x' || (i % 100) FROM generate_series(1, 2000000) i;
CREATE TABLE gk (k int4, v int8);
INSERT INTO gk SELECT i, i * 10 FROM generate_series(0, 999) i;
CREATE TABLE gt (t text, v int4);
INSERT INTO gt SELECT 'row' || i, i FROM generate_series(1, 1000) i;
CREATE TABLE gm (id int4, a int4, n numeric(10,2), m numeric(6,3));
INSERT INTO gm SELECT i, (i * 7) % 23, ((i * 37) % 100000) / 100.0, ((i * 7) % 1000) / 1000.0 + 1 FROM generate_series(1, 2000000) i;
VACUUM ANALYZE gm;
CREATE TABLE gd (id int4, a int4, ts timestamp, tz timestamptz, d date);
INSERT INTO gd SELECT i, (i * 7) % 23, timestamp '2024-01-01' + ((i % 730) || ' days')::interval + ((i::int8 * 7919) % 86400 || ' seconds')::interval + ((i * 13) % 1000000 || ' microseconds')::interval, timestamptz '2024-01-01 00:00:00+00' + ((i % 730) || ' days')::interval + ((i::int8 * 7919) % 86400 || ' seconds')::interval, date '2024-01-01' + (i % 730) FROM generate_series(1, 2000000) i;
VACUUM ANALYZE gd;
VACUUM ANALYZE gn;
VACUUM ANALYZE gw;
VACUUM ANALYZE gk;
VACUUM ANALYZE gt;
CREATE TABLE gp (id int4, a int4, b int8, c float8, t text, f bool);
INSERT INTO gp SELECT i, (i * 7) % 1000, i::int8 * 1000, i / 4.0, 'row' || i, i % 3 = 0 FROM generate_series(1, 2000000) i;
CREATE TABLE gr (id int4, a int4, b int8, c float8, t text, f bool);
INSERT INTO gr SELECT i, (hashint4(i) & 2147483647) % 1000, i::int8 * 1000, i / 4.0, 'row' || ((hashint4(i * 3) & 2147483647) % 2000000), (hashint4(i * 5) & 1) = 1 FROM generate_series(1, 2000000) i;
VACUUM ANALYZE gp;
VACUUM ANALYZE gr;
CREATE ACCESS METHOD pgrcolumnar TYPE TABLE HANDLER heap_tableam_handler;
CREATE TABLE gch (id int8, a int4, b int4, c int4, dt text COLLATE "C", j jsonb);
INSERT INTO gch SELECT i, (i * 7) % 23, (i * 13) % 1000, (i * 11) % 23, (ARRAY['alpha','beta','gamma','delta','epsilon','zeta','eta'])[(i % 7) + 1], jsonb_build_object('k', (ARRAY['alpha','beta','gamma','delta','epsilon','zeta','eta'])[(i % 7) + 1], 'n', i) FROM generate_series(1, 2000000) i;
CREATE TABLE gc (id int8, a int4, b int4, c int4, dt text COLLATE "C", j jsonb) USING pgrcolumnar;
\set gc_tsv :work '/bench-gc.tsv'
COPY gch TO :'gc_tsv';
COPY gc FROM :'gc_tsv';
CREATE TABLE gcs (id int8, a int4, b int4, c int4, dt text COLLATE "C", j jsonb) USING pgrcolumnar;
\set gcs_tsv :work '/bench-gcs.tsv'
COPY (SELECT * FROM gch ORDER BY dt, a, id) TO :'gcs_tsv';
COPY gcs FROM :'gcs_tsv';
ANALYZE gcs;
ANALYZE gch;
ANALYZE gc;
CREATE TABLE sv (site_id int4, observed_at int8, observer_id int8, method_id int2, notes text COLLATE "C", media_url text COLLATE "C", taxon_label text COLLATE "C", source_url text COLLATE "C");
INSERT INTO sv SELECT (h1 % 100)::int4, h2::int8, (h3 % 100000)::int8, (i % 5)::int2, CASE WHEN h4 % 100 < 86 THEN '' ELSE 'phrase ' || (h5 % 50000) END, 'https://media' || (h6 % 200000) || '.test/' || CASE WHEN h7 % 100 < 8 THEN 'atlas' ELSE 'm' END || '/' || (i % 97), 'Taxon ' || (h8 % 30000) || CASE WHEN h9 % 100 < 4 THEN ' Heron' ELSE '' END, CASE WHEN h10 % 100 < 70 THEN 'http://' || CASE WHEN h11 % 2 = 0 THEN 'www.' ELSE '' END || 'src' || (h12 % 1000) || '.test/p' || (i % 13) ELSE '' END FROM (SELECT i, hashint4(i) & 2147483647 AS h1, hashint4(i * 2) & 2147483647 AS h2, hashint4(i * 3) & 2147483647 AS h3, hashint4(i * 4) & 2147483647 AS h4, hashint4(i * 5) & 2147483647 AS h5, hashint4(i * 6) & 2147483647 AS h6, hashint4(i * 7) & 2147483647 AS h7, hashint4(i * 8) & 2147483647 AS h8, hashint4(i * 9) & 2147483647 AS h9, hashint4(i * 10) & 2147483647 AS h10, hashint4(i * 11) & 2147483647 AS h11, hashint4(i * 12) & 2147483647 AS h12 FROM generate_series(1, 2000000) i) s;
CREATE TABLE svc (site_id int4, observed_at int8, observer_id int8, method_id int2, notes text COLLATE "C", media_url text COLLATE "C", taxon_label text COLLATE "C", source_url text COLLATE "C") USING pgrcolumnar;
\set sv_tsv :work '/bench-sv.tsv'
COPY sv TO :'sv_tsv';
COPY svc FROM :'sv_tsv';
VACUUM ANALYZE sv;
ANALYZE svc;
CREATE FUNCTION p1() RETURNS int8 LANGUAGE plpgsql AS $$ DECLARE s int8 := 0; r record; BEGIN FOR r IN SELECT id, a + 1 AS a1 FROM g WHERE a > 15 LOOP s := s + r.a1; END LOOP; RETURN s; END $$;
CREATE FUNCTION p2() RETURNS TABLE(id int4, a1 int4) LANGUAGE plpgsql AS $$ BEGIN RETURN QUERY SELECT g.id, g.a + 1 FROM g WHERE g.a > 15; END $$;
CREATE FUNCTION p3() RETURNS int8 LANGUAGE plpgsql AS $$ DECLARE s int8; BEGIN SELECT sum(a + 1) INTO s FROM g WHERE a > 15; RETURN s; END $$;
CREATE FUNCTION p4() RETURNS int8 LANGUAGE plpgsql AS $$ DECLARE s int8 := 0; v int4; BEGIN FOR i IN 1..100000 LOOP SELECT a INTO v FROM g WHERE id = i; s := s + v; END LOOP; RETURN s; END $$;
SET debug_print_plan = on; SET max_parallel_workers_per_gather = 0; SET jit = off;
-- Q1: filter + project, ~30% selectivity
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = on; SET enable_hashjoin = on; SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_incremental_sort = off;
EXPLAIN (COSTS OFF) SELECT id, a + 1 FROM g WHERE a > 15 /* Q1 */;
-- Q2: filter, no rows pass (pure scan + deform + filter cost)
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = on; SET enable_hashjoin = on; SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_incremental_sort = off;
EXPLAIN (COSTS OFF) SELECT id FROM g WHERE a > 100 /* Q2 */;
-- Q3: wide filter on int8 and float8
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = on; SET enable_hashjoin = on; SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_incremental_sort = off;
EXPLAIN (COSTS OFF) SELECT id, b, c FROM g WHERE b > 1000000 AND c < 400000.0 /* Q3 */;
-- Q4: all rows, unprojected
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = on; SET enable_hashjoin = on; SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_incremental_sort = off;
EXPLAIN (COSTS OFF) SELECT * FROM g WHERE id > 0 /* Q4 */;
-- Q5: Sort breaker below a bounded Limit (multi-pipeline: filter+project producer, Rust-owned sort, per-row consumer)
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = on; SET enable_hashjoin = on; SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_incremental_sort = off;
EXPLAIN (COSTS OFF) SELECT id FROM g WHERE a > 5 ORDER BY b LIMIT 10 /* Q5 */;
-- Q6: Sort breaker, all rows through the per-row consumer (the consumer-side per-row cost)
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = on; SET enable_hashjoin = on; SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_incremental_sort = off;
EXPLAIN (COSTS OFF) SELECT id FROM g WHERE a > 15 ORDER BY b DESC LIMIT 300000 /* Q6 */;
-- Q7: HashAgg breaker, 23 groups: generated consume loop (hash, probe, count/sum updates)
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = on; SET enable_hashjoin = on; SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_incremental_sort = off;
EXPLAIN (COSTS OFF) SELECT a, count(*), sum(b) FROM g GROUP BY a ORDER BY a /* Q7 */;
-- Q8: HashAgg breaker, 100k groups (int4 key expression; the table leaves L1)
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = on; SET enable_hashjoin = on; SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_incremental_sort = off;
EXPLAIN (COSTS OFF) SELECT id % 100000 AS k, count(*), sum(b) FROM g GROUP BY id % 100000 ORDER BY k LIMIT 5 /* Q8 */;
-- Q9: hash join, 23 build rows: generated probe (tagged chain walk), every outer row matches
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = on; SET enable_hashjoin = on; SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_incremental_sort = off;
EXPLAIN (COSTS OFF) SELECT g2.k, count(*) FROM g JOIN g2 ON g.a = g2.k GROUP BY g2.k ORDER BY g2.k /* Q9 */;
-- Q10: hash join, 100k build rows: generated probe, 1 in 20 outer rows matches
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = on; SET enable_hashjoin = on; SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_incremental_sort = off;
EXPLAIN (COSTS OFF) SELECT g3.k % 7 AS k, count(*) FROM g JOIN g3 ON g.id = g3.k GROUP BY g3.k % 7 ORDER BY k /* Q10 */;
-- Q11: Unique op over a Sort breaker (sorted DISTINCT, 23 runs): the per-row compare against the kept key
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = off; SET enable_hashjoin = on; SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_incremental_sort = off;
EXPLAIN (COSTS OFF) SELECT DISTINCT a FROM g /* Q11 */;
-- Q12: sorted Agg over a Sort breaker (GroupAggregate, 23 groups): open-coded count/sum updates per row
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = off; SET enable_hashjoin = on; SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_incremental_sort = off;
EXPLAIN (COSTS OFF) SELECT a, count(*), sum(b) FROM g GROUP BY a ORDER BY a /* Q12 */;
-- Q13: NestLoop op, 23-row Material inner rewound per outer row, feeding a HashAgg
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = on; SET enable_hashjoin = off; SET enable_nestloop = on; SET enable_mergejoin = off; SET enable_incremental_sort = off;
EXPLAIN (COSTS OFF) SELECT g2.k, count(*) FROM g JOIN g2 ON g.a = g2.k GROUP BY g2.k ORDER BY g2.k /* Q13 */;
-- Q14: MergeJoin op over two Sorts (2M x 100k, unique keys), feeding a HashAgg
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = on; SET enable_hashjoin = off; SET enable_nestloop = off; SET enable_mergejoin = on; SET enable_incremental_sort = off;
EXPLAIN (COSTS OFF) SELECT g3.k % 7 AS k, count(*) FROM g JOIN g3 ON g.id = g3.k GROUP BY g3.k % 7 ORDER BY k /* Q14 */;
-- Q15: WindowAgg breaker (the hosted window node): row_number() over 2M rows, feeding a HashAgg
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = on; SET enable_hashjoin = on; SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_incremental_sort = off;
EXPLAIN (COSTS OFF) SELECT rn % 7 AS k, count(*) FROM (SELECT id, row_number() OVER (ORDER BY b) AS rn FROM g) s GROUP BY rn % 7 ORDER BY k /* Q15 */;
-- Q16: HashAgg breaker, 20k groups in runs of 100 consecutive rows (the adjacent-duplicate skip)
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = on; SET enable_hashjoin = on; SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_incremental_sort = off;
EXPLAIN (COSTS OFF) SELECT id / 100 AS k, count(*), sum(b) FROM g GROUP BY id / 100 ORDER BY k LIMIT 5 /* Q16 */;
-- Q17: distinct-only HashAgg, 100k distinct int4 keys (the bijective distinct table), counted by a HashAgg above
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = on; SET enable_hashjoin = on; SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_incremental_sort = off;
EXPLAIN (COSTS OFF) SELECT k % 7 AS m, count(*) FROM (SELECT id % 100000 AS k FROM g GROUP BY id % 100000) s GROUP BY k % 7 ORDER BY 2, m /* Q17 */;
-- Q18: IndexScan source (per-row source_next over the incumbent's node), 600k index rows, no row passes the filter (the per-row contract cost)
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = on; SET enable_hashjoin = on; SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_incremental_sort = off; SET enable_indexscan = on;
EXPLAIN (COSTS OFF) SELECT id, a + 1 FROM g WHERE id BETWEEN 500000 AND 1100000 AND a > 100 /* Q18 */;
-- Q19: IndexScan source, 600k rows projected through the per-row path
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = on; SET enable_hashjoin = on; SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_incremental_sort = off; SET enable_indexscan = on;
EXPLAIN (COSTS OFF) SELECT id, a + 1 FROM g WHERE id BETWEEN 500000 AND 1100000 /* Q19 */;
-- Q20: PlainAgg breaker: count(*), sum(b) over a 30% filter (one group on the HashAgg lane)
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = on; SET enable_hashjoin = on; SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_incremental_sort = off;
EXPLAIN (COSTS OFF) SELECT count(*), sum(b) FROM g WHERE a > 15 /* Q20 */;
-- C1: columnar source: count(*) WHERE a > c (two-column zone qual; PlainAgg over granule lanes)
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = on; SET enable_hashjoin = on; SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_incremental_sort = off;
EXPLAIN (COSTS OFF) SELECT count(*) FROM gc WHERE a > c /* C1 */;
-- C1h: the heap twin of C1 (paged heap source)
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = on; SET enable_hashjoin = on; SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_incremental_sort = off;
EXPLAIN (COSTS OFF) SELECT count(*) FROM gch WHERE a > c /* C1h */;
-- C2: columnar source: GROUP BY dictionary text, 7 groups (HashAgg, text keys by fingerprint)
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = on; SET enable_hashjoin = on; SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_incremental_sort = off;
EXPLAIN (COSTS OFF) SELECT dt, count(*) FROM gc GROUP BY dt ORDER BY dt /* C2 */;
-- C2h: the heap twin of C2
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = on; SET enable_hashjoin = on; SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_incremental_sort = off;
EXPLAIN (COSTS OFF) SELECT dt, count(*) FROM gch GROUP BY dt ORDER BY dt /* C2h */;
-- C3: columnar source: a, sum(b) GROUP BY a, 23 groups (HashAgg, open-coded int8 sum)
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = on; SET enable_hashjoin = on; SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_incremental_sort = off;
EXPLAIN (COSTS OFF) SELECT a, sum(b) FROM gc GROUP BY a ORDER BY a /* C3 */;
-- C3h: the heap twin of C3
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = on; SET enable_hashjoin = on; SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_incremental_sort = off;
EXPLAIN (COSTS OFF) SELECT a, sum(b) FROM gch GROUP BY a ORDER BY a /* C3h */;
-- C4: columnar source: count(*) WHERE j->>'k' = 'v' on a shredded jsonb Text lane
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = on; SET enable_hashjoin = on; SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_incremental_sort = off;
EXPLAIN (COSTS OFF) SELECT count(*) FROM gc WHERE j->>'k' = 'gamma' /* C4 */;
-- C4h: the heap twin of C4 (jsonb_object_field_text per row)
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = on; SET enable_hashjoin = on; SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_incremental_sort = off;
EXPLAIN (COSTS OFF) SELECT count(*) FROM gch WHERE j->>'k' = 'gamma' /* C4h */;
-- C5: columnar source: GROUP BY j->>'k' on the lane (HashAgg, text keys)
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = on; SET enable_hashjoin = on; SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_incremental_sort = off;
EXPLAIN (COSTS OFF) SELECT j->>'k' AS k, count(*) FROM gc GROUP BY 1 ORDER BY 1 /* C5 */;
-- C5h: the heap twin of C5
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = on; SET enable_hashjoin = on; SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_incremental_sort = off;
EXPLAIN (COSTS OFF) SELECT j->>'k' AS k, count(*) FROM gch GROUP BY 1 ORDER BY 1 /* C5h */;
-- Q21: Memoize breaker (the hosted cache) in front of a NestLoop inner: 2M outer rows, 23 distinct keys (23 misses, the rest hits), an index-only scan of the 23-row g4 below, feeding a HashAgg (serve-c.md)
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = on; SET enable_hashjoin = off; SET enable_nestloop = on; SET enable_mergejoin = off; SET enable_incremental_sort = off; SET enable_memoize = on;
EXPLAIN (COSTS OFF) SELECT g4.k, count(*) FROM g JOIN g4 ON g.a = g4.k GROUP BY g4.k ORDER BY g4.k /* Q21 */;
-- Q22: RecursiveUnion breaker: WITH RECURSIVE counting to 2M (one row per iteration: the recursive term rescanned and re-pulled per iteration), count(*) above (serve-c.md)
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = on; SET enable_hashjoin = on; SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_incremental_sort = off;
EXPLAIN (COSTS OFF) WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM t WHERE n < 2000000) SELECT count(*) FROM t /* Q22 */;
-- D1: count(DISTINCT a): PlainAgg with a DISTINCT aggregate over a word key (the per-group identity set, aggregates-pass.md)
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = on; SET enable_hashjoin = on; SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_incremental_sort = off;
EXPLAIN (COSTS OFF) SELECT count(DISTINCT a) FROM g /* D1 */;
-- D2: count(DISTINCT t): the same over the text fingerprint (a distinct count over text)
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = on; SET enable_hashjoin = on; SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_incremental_sort = off;
EXPLAIN (COSTS OFF) SELECT count(DISTINCT t) FROM g /* D2 */;
-- Q25: Hash Right Join, 23 build rows (the preserved side; every key matches, the drain walks 23 matched entries), feeding a HashAgg: Q9 as a right join
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = on; SET enable_hashjoin = on; SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_incremental_sort = off;
EXPLAIN (COSTS OFF) SELECT g2.k, count(g.id) FROM g2 LEFT JOIN g ON g.a = g2.k GROUP BY g2.k ORDER BY g2.k /* Q25 */;
-- Q26: Hash Right Join, 100k build rows, 1 in 20 outer rows matches, one build row drained, feeding a HashAgg: Q10 as a right join
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = on; SET enable_hashjoin = on; SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_incremental_sort = off;
EXPLAIN (COSTS OFF) SELECT g3.k % 7 AS k, count(g.id) FROM g3 LEFT JOIN g ON g.id = g3.k GROUP BY g3.k % 7 ORDER BY k /* Q26 */;
-- Q28: Hash Full Join, 100k build rows (g3), 1 in 20 outer rows matches, 1.9M outer rows null-filled (the LEFT fill), one build row drained, feeding a HashAgg: Q10 as a full join (serve-d.md)
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = on; SET enable_hashjoin = on; SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_incremental_sort = off;
EXPLAIN (COSTS OFF) SELECT g3.k % 7 AS k, count(g.id) FROM g3 FULL JOIN g ON g.id = g3.k GROUP BY g3.k % 7 ORDER BY k /* Q28 */;
-- Q27: the generated window op (serve-d.md): a running sum(a) OVER (ORDER BY b ROWS UNBOUNDED PRECEDING) over 2M sorted rows, feeding a HashAgg
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = on; SET enable_hashjoin = on; SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_incremental_sort = off;
EXPLAIN (COSTS OFF) SELECT s % 1000 AS k, count(*) FROM (SELECT sum(a) OVER (ORDER BY b ROWS UNBOUNDED PRECEDING) AS s FROM g) t GROUP BY 1 ORDER BY 1 LIMIT 5 /* Q27 */;
-- C6: columnar source: count(*) WHERE id = const on the sorted id (zone map keeps one granule, its PSMA block narrows it to one bucket: columnar-encoded-lanes.md §3)
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = on; SET enable_hashjoin = on; SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_incremental_sort = off;
EXPLAIN (COSTS OFF) SELECT count(*) FROM gc WHERE id = 1234567 /* C6 */;
-- C7: columnar source: count(*), sum(b) WHERE id BETWEEN x AND x + 4000 (two range quals, one granule, PSMA windows intersected)
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = on; SET enable_hashjoin = on; SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_incremental_sort = off;
EXPLAIN (COSTS OFF) SELECT count(*), sum(b) FROM gc WHERE id BETWEEN 1000000 AND 1004000 /* C7 */;
-- C8: columnar source: count(*) WHERE b = 500 on an unclustered column (the consult narrows nothing: the control)
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = on; SET enable_hashjoin = on; SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_incremental_sort = off;
EXPLAIN (COSTS OFF) SELECT count(*) FROM gc WHERE b = 500 /* C8 */;
-- P02: regime P02: id, a+1 WHERE a < 20 on gp (periodic a, 2%): kernel branching
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = on; SET enable_hashjoin = on; SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_incremental_sort = off;
EXPLAIN (COSTS OFF) SELECT id, a + 1 FROM gp WHERE a < 20 /* P02 */;
-- P30: regime P30: id, a+1 WHERE a > 699 on gp (periodic a, 30%; Q1's shape at period 1000): kernel branching
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = on; SET enable_hashjoin = on; SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_incremental_sort = off;
EXPLAIN (COSTS OFF) SELECT id, a + 1 FROM gp WHERE a > 699 /* P30 */;
-- P98: regime P98: id, a+1 WHERE a >= 20 on gp (periodic a, 98%): kernel branching
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = on; SET enable_hashjoin = on; SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_incremental_sort = off;
EXPLAIN (COSTS OFF) SELECT id, a + 1 FROM gp WHERE a >= 20 /* P98 */;
-- R02: regime R02: id, a+1 WHERE a < 20 on gr (random a, 2%): kernel branching
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = on; SET enable_hashjoin = on; SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_incremental_sort = off;
EXPLAIN (COSTS OFF) SELECT id, a + 1 FROM gr WHERE a < 20 /* R02 */;
-- R30: regime R30: id, a+1 WHERE a > 699 on gr (random a, 30%): kernel predicated (selection vector per page)
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = on; SET enable_hashjoin = on; SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_incremental_sort = off;
EXPLAIN (COSTS OFF) SELECT id, a + 1 FROM gr WHERE a > 699 /* R30 */;
-- R98: regime R98: id, a+1 WHERE a >= 20 on gr (random a, 98%): kernel branching
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = on; SET enable_hashjoin = on; SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_incremental_sort = off;
EXPLAIN (COSTS OFF) SELECT id, a + 1 FROM gr WHERE a >= 20 /* R98 */;
-- S1r: random twin of S1: id, b WHERE a < 43 on gr (4.3%): kernel branching
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = on; SET enable_hashjoin = on; SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_incremental_sort = off;
EXPLAIN (COSTS OFF) SELECT id, b FROM gr WHERE a < 43 /* S1r */;
-- S2r: random twin of S2: id, b WHERE a >= 43 on gr (95.7%): kernel branching
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = on; SET enable_hashjoin = on; SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_incremental_sort = off;
EXPLAIN (COSTS OFF) SELECT id, b FROM gr WHERE a >= 43 /* S2r */;
-- S3r: random twin of S3: id, length(t), upper(t) WHERE a < 43 on gr (4.3%): kernel branching
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = on; SET enable_hashjoin = on; SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_incremental_sort = off;
EXPLAIN (COSTS OFF) SELECT id, length(t), upper(t) FROM gr WHERE a < 43 /* S3r */;
-- S4r: random twin of S4: id, c * 2.5 + b::float8 WHERE f on gr (random f, 50%): kernel predicated
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = on; SET enable_hashjoin = on; SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_incremental_sort = off;
EXPLAIN (COSTS OFF) SELECT id, c * 2.5 + b::float8 AS v FROM gr WHERE f /* S4r */;
-- S5r: random twin of S5: count(*) WHERE t LIKE 'row1%' on gr (random t, ~11%): kernel predicated (count += predicate)
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = on; SET enable_hashjoin = on; SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_incremental_sort = off;
EXPLAIN (COSTS OFF) SELECT count(*) FROM gr WHERE t LIKE 'row1%' /* S5r */;
-- S1: corpus S1: filter + 2-col project, 4.3% selectivity (the filter dominates)
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = on; SET enable_hashjoin = on; SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_incremental_sort = off;
EXPLAIN (COSTS OFF) SELECT id, b FROM g WHERE a > 21 /* S1 */;
-- S2: corpus S2: filter + 2-col project, 96% selectivity (the emit path dominates)
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = on; SET enable_hashjoin = on; SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_incremental_sort = off;
EXPLAIN (COSTS OFF) SELECT id, b FROM g WHERE a >= 1 /* S2 */;
-- S3: corpus S3: text fmgr in the projection (length, upper) at 4.3%
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = on; SET enable_hashjoin = on; SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_incremental_sort = off;
EXPLAIN (COSTS OFF) SELECT id, length(t), upper(t) FROM g WHERE a = 3 /* S3 */;
-- S4: corpus S4: float8 arithmetic in the projection, a bool filter past the varlena (33%)
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = on; SET enable_hashjoin = on; SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_incremental_sort = off;
EXPLAIN (COSTS OFF) SELECT id, c * 2.5 + b::float8 AS v FROM g WHERE f /* S4 */;
-- S5: corpus S5: count(*) under a LIKE prefix predicate (textlike per row)
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = on; SET enable_hashjoin = on; SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_incremental_sort = off;
EXPLAIN (COSTS OFF) SELECT count(*) FROM g WHERE t LIKE 'row1%' /* S5 */;
-- S6: corpus S6: count(*) under a text equality predicate (texteq per row)
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = on; SET enable_hashjoin = on; SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_incremental_sort = off;
EXPLAIN (COSTS OFF) SELECT count(*) FROM g WHERE t = 'row777777' /* S6 */;
-- W1: corpus W1: wide row (18 attributes), two mid columns projected at 30%
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = on; SET enable_hashjoin = on; SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_incremental_sort = off;
EXPLAIN (COSTS OFF) SELECT c3, c7 FROM gw WHERE c1 > 699 /* W1 */;
-- W2: corpus W2: wide row, PlainAgg over the late attributes past the int8 alignment gap (50%)
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = on; SET enable_hashjoin = on; SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_incremental_sort = off;
EXPLAIN (COSTS OFF) SELECT count(*), sum(d1), max(c12) FROM gw WHERE c11 > 499 /* W2 */;
-- N1: corpus N1: NULL-heavy rows, PlainAgg with strict count/sum (the bitmap deform on the hot path)
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = on; SET enable_hashjoin = on; SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_incremental_sort = off;
EXPLAIN (COSTS OFF) SELECT count(*), count(a), count(t), sum(b) FROM gn /* N1 */;
-- N2: corpus N2: NULL-heavy rows, filter on a nullable column, a nullable column emitted
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = on; SET enable_hashjoin = on; SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_incremental_sort = off;
EXPLAIN (COSTS OFF) SELECT id, b FROM gn WHERE a > 15 /* N2 */;
-- N3: corpus N3: NULL-heavy rows, HashAgg with a NULL key group and NULL sum inputs
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = on; SET enable_hashjoin = on; SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_incremental_sort = off;
EXPLAIN (COSTS OFF) SELECT a, count(*), sum(b) FROM gn GROUP BY a ORDER BY a /* N3 */;
-- A1: corpus A1: PlainAgg min/max(int8) + sum(float8)
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = on; SET enable_hashjoin = on; SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_incremental_sort = off;
EXPLAIN (COSTS OFF) SELECT min(b), max(b), sum(c) FROM g /* A1 */;
-- A2: corpus A2: HashAgg 1000 groups (expression key), count + sum, top-5 keys
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = on; SET enable_hashjoin = on; SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_incremental_sort = off;
EXPLAIN (COSTS OFF) SELECT id % 1000 AS k, count(*), sum(b) FROM g GROUP BY 1 ORDER BY 1 LIMIT 5 /* A2 */;
-- A3: corpus A3: HashAgg on two keys (int4, bool), 46 groups
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = on; SET enable_hashjoin = on; SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_incremental_sort = off;
EXPLAIN (COSTS OFF) SELECT a, f, count(*) FROM g GROUP BY a, f ORDER BY a, f /* A3 */;
-- A4: corpus A4: HashAgg on a text key computed by substr(), 9 groups
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = on; SET enable_hashjoin = on; SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_incremental_sort = off;
EXPLAIN (COSTS OFF) SELECT substr(t, 1, 4) AS k, count(*) FROM g GROUP BY 1 ORDER BY 1 /* A4 */;
-- A5: corpus A5: HashAgg 23 groups, min/max(int8) + max(float8)
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = on; SET enable_hashjoin = on; SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_incremental_sort = off;
EXPLAIN (COSTS OFF) SELECT a, min(b), max(b), max(c) FROM g GROUP BY a ORDER BY a /* A5 */;
-- A6: corpus A6: HashAgg 1M groups (every probe a cache miss), top-5 keys
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = on; SET enable_hashjoin = on; SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_incremental_sort = off;
EXPLAIN (COSTS OFF) SELECT id / 2 AS k, count(*) FROM g GROUP BY 1 ORDER BY 1 LIMIT 5 /* A6 */;
-- A7: corpus A7: HashAgg 5 groups with count(DISTINCT b) per group (identity sets)
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = on; SET enable_hashjoin = on; SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_incremental_sort = off;
EXPLAIN (COSTS OFF) SELECT a % 5 AS k, count(DISTINCT b) FROM g GROUP BY 1 ORDER BY 1 /* A7 */;
-- O1: corpus O1: bounded Sort (10) on a text key
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = on; SET enable_hashjoin = on; SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_incremental_sort = off;
EXPLAIN (COSTS OFF) SELECT id, t FROM g ORDER BY t LIMIT 10 /* O1 */;
-- O2: corpus O2: bounded Sort (10) on two keys (a ASC, b DESC)
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = on; SET enable_hashjoin = on; SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_incremental_sort = off;
EXPLAIN (COSTS OFF) SELECT id FROM g ORDER BY a, b DESC LIMIT 10 /* O2 */;
-- O3: corpus O3: bounded Sort (1000) on b with a text payload, under an 8.7% filter
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = on; SET enable_hashjoin = on; SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_incremental_sort = off;
EXPLAIN (COSTS OFF) SELECT id, t FROM g WHERE a > 20 ORDER BY b LIMIT 1000 /* O3 */;
-- J1: corpus J1: hash SEMI join, 12-row build (IN subquery)
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = on; SET enable_hashjoin = on; SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_incremental_sort = off;
EXPLAIN (COSTS OFF) SELECT count(*) FROM g WHERE a IN (SELECT k FROM g2 WHERE k > 10) /* J1 */;
-- J2: corpus J2: hash ANTI join, 100k build (NOT EXISTS), 95% of probes miss
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = on; SET enable_hashjoin = on; SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_incremental_sort = off;
EXPLAIN (COSTS OFF) SELECT count(*) FROM g WHERE NOT EXISTS (SELECT 1 FROM g3 WHERE g3.k = g.id) /* J2 */;
-- J3: corpus J3: hash LEFT join, outer preserved, 12 of 23 build keys match
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = on; SET enable_hashjoin = on; SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_incremental_sort = off;
EXPLAIN (COSTS OFF) SELECT count(*), count(g2.v) FROM g LEFT JOIN g2 ON g.a = g2.k AND g2.v > 100 /* J3 */;
-- J4: corpus J4: hash join projecting the build payload, 8.7% out
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = on; SET enable_hashjoin = on; SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_incremental_sort = off;
EXPLAIN (COSTS OFF) SELECT g.id, g2.v FROM g JOIN g2 ON g.a = g2.k WHERE g2.k > 20 /* J4 */;
-- J5: corpus J5: hash join, 1000-row analyzed build, expression probe key, HashAgg above
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = on; SET enable_hashjoin = on; SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_incremental_sort = off;
EXPLAIN (COSTS OFF) SELECT gk.k % 10 AS m, count(*), sum(gk.v) FROM g JOIN gk ON g.id % 1000 = gk.k GROUP BY 1 ORDER BY 1 /* J5 */;
-- J6: corpus J6: MergeJoin over two Sorts with duplicate runs (2M x 23)
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = on; SET enable_hashjoin = off; SET enable_nestloop = off; SET enable_mergejoin = on; SET enable_incremental_sort = off;
EXPLAIN (COSTS OFF) SELECT count(*) FROM g JOIN g2 ON g.a = g2.k /* J6 */;
-- J7: corpus J7: two hash probes in one pipeline (g2, g4)
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = on; SET enable_hashjoin = on; SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_incremental_sort = off;
EXPLAIN (COSTS OFF) SELECT count(*) FROM g JOIN g2 ON g.a = g2.k JOIN g4 ON g4.k = g2.k WHERE g4.v > 50 /* J7 */;
-- J8: corpus J8: hash join on a text key, 1000-row build (fingerprint lane)
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = on; SET enable_hashjoin = on; SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_incremental_sort = off;
EXPLAIN (COSTS OFF) SELECT count(*), sum(gt.v) FROM g JOIN gt ON g.t = gt.t /* J8 */;
-- WG1: corpus WG1: served window op rank() with PARTITION BY a ORDER BY b, count(r <= 3)
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = on; SET enable_hashjoin = on; SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_incremental_sort = off;
EXPLAIN (COSTS OFF) SELECT count(*) FROM (SELECT rank() OVER (PARTITION BY a ORDER BY b) AS r FROM g) s WHERE r <= 3 /* WG1 */;
-- WG2: corpus WG2: window op max(b) OVER (PARTITION BY a) (whole-partition look-ahead), count(b = m)
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = on; SET enable_hashjoin = on; SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_incremental_sort = off;
EXPLAIN (COSTS OFF) SELECT count(*) FROM (SELECT b, max(b) OVER (PARTITION BY a) AS m FROM g) s WHERE b = m /* WG2 */;
-- WG3: corpus WG3: count(*) OVER (PARTITION BY a) under a partition-only Sort (window-hash.md: the partition kernel, no row sort), min / max of the counts
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = on; SET enable_hashjoin = on; SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_incremental_sort = off;
EXPLAIN (COSTS OFF) SELECT max(c) AS mx, min(c) AS mn FROM (SELECT count(*) OVER (PARTITION BY a) AS c FROM g) s /* WG3 */;
-- WG4: corpus WG4: max(d1) OVER (PARTITION BY t2, c1) over gw (a text + int partition pair, 1000 partitions, the partition kernel), count(d1 = m)
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = on; SET enable_hashjoin = on; SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_incremental_sort = off;
EXPLAIN (COSTS OFF) SELECT count(*) FROM (SELECT d1, max(d1) OVER (PARTITION BY t2, c1) AS m FROM gw) s WHERE d1 = m /* WG4 */;
-- U1: corpus U1: Append (UNION ALL) of a filtered g and g3, count(*)
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = on; SET enable_hashjoin = on; SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_incremental_sort = off;
EXPLAIN (COSTS OFF) SELECT count(*) FROM (SELECT id FROM g WHERE a > 15 UNION ALL SELECT k FROM g3) s /* U1 */;
-- U2: corpus U2: HashSetOp EXCEPT over the 23-key domain
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = on; SET enable_hashjoin = on; SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_incremental_sort = off;
EXPLAIN (COSTS OFF) SELECT count(*) FROM (SELECT a FROM g EXCEPT SELECT k FROM g2 WHERE k < 10) s /* U2 */;
-- U3: corpus U3: MergeAppend over two bounded Sorts, LIMIT 10
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = on; SET enable_hashjoin = on; SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_incremental_sort = off;
EXPLAIN (COSTS OFF) SELECT id FROM (SELECT id FROM g WHERE a > 20 UNION ALL SELECT k FROM g3) s ORDER BY id LIMIT 10 /* U3 */;
-- P1: corpus P1: ProjectSet generate_series(1, a % 3) per row, count(*)
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = on; SET enable_hashjoin = on; SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_incremental_sort = off;
EXPLAIN (COSTS OFF) SELECT count(*) FROM (SELECT generate_series(1, a % 3) FROM g) s /* P1 */;
-- R1: corpus R1: RecursiveUnion with 23 rows per iteration (4348 iterations)
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = on; SET enable_hashjoin = on; SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_incremental_sort = off;
EXPLAIN (COSTS OFF) WITH RECURSIVE t(k) AS (SELECT k FROM g2 UNION ALL SELECT k + 23 FROM t WHERE k + 23 < 100000) SELECT count(*) FROM t /* R1 */;
-- R2: corpus R2: RecursiveUnion UNION (dedup) closing the orbit of x -> 7919 x mod 100003 from 23 seeds
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = on; SET enable_hashjoin = on; SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_incremental_sort = off;
EXPLAIN (COSTS OFF) WITH RECURSIVE t(n) AS (SELECT k FROM g2 UNION SELECT (n * 7919) % 100003 FROM t) SELECT count(*) FROM t /* R2 */;
-- R3: corpus R3: RecursiveUnion whose term probes g2 (built before the loop), 23 chains to 100000
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = on; SET enable_hashjoin = on; SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_incremental_sort = off;
EXPLAIN (COSTS OFF) WITH RECURSIVE t(k) AS (SELECT k FROM g2 UNION ALL SELECT t.k + g2.k + 1 FROM t JOIN g2 ON g2.k = t.k % 23 WHERE t.k < 100000) SELECT count(*) FROM t /* R3 */;
-- C10: corpus C10 (was C6): columnar zone-map skip, constant range on the sorted id (count)
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = on; SET enable_hashjoin = on; SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_incremental_sort = off;
EXPLAIN (COSTS OFF) SELECT count(*) FROM gc WHERE id BETWEEN 1000000 AND 1010000 /* C10 */;
-- C10h: the heap twin of C10
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = on; SET enable_hashjoin = on; SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_incremental_sort = off;
EXPLAIN (COSTS OFF) SELECT count(*) FROM gch WHERE id BETWEEN 1000000 AND 1010000 /* C10h */;
-- C11: corpus C11 (was C7): columnar PlainAgg sum(b) under a lane filter a > 11
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = on; SET enable_hashjoin = on; SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_incremental_sort = off;
EXPLAIN (COSTS OFF) SELECT sum(b) FROM gc WHERE a > 11 /* C11 */;
-- C11h: the heap twin of C11
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = on; SET enable_hashjoin = on; SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_incremental_sort = off;
EXPLAIN (COSTS OFF) SELECT sum(b) FROM gch WHERE a > 11 /* C11h */;
-- C12: corpus C12 (was C8): columnar jsonb path lane IN-list filter feeding an int HashAgg
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = on; SET enable_hashjoin = on; SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_incremental_sort = off;
EXPLAIN (COSTS OFF) SELECT a, count(*) FROM gc WHERE j->>'k' IN ('gamma', 'beta') GROUP BY a ORDER BY a /* C12 */;
-- C12h: the heap twin of C12 (jsonb_object_field_text per row)
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = on; SET enable_hashjoin = on; SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_incremental_sort = off;
EXPLAIN (COSTS OFF) SELECT a, count(*) FROM gch WHERE j->>'k' IN ('gamma', 'beta') GROUP BY a ORDER BY a /* C12h */;
-- C13: corpus C13 (was C9): bounded Sort over the columnar source (b DESC, id), LIMIT 10
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = on; SET enable_hashjoin = on; SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_incremental_sort = off;
EXPLAIN (COSTS OFF) SELECT id FROM gc ORDER BY b DESC, id LIMIT 10 /* C13 */;
-- PL1: plpgsql FOR loop over the Q1 rows (SPI cursor portal, 50-row fetches: the count-limited run path), a running sum (output-path.md §4)
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = on; SET enable_hashjoin = on; SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_incremental_sort = off;
SELECT p1() /* PL1 */;
-- PL2: plpgsql RETURN QUERY of the Q1 rows (SPI tcount 0 into the tuplestore, then read back), counted above
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = on; SET enable_hashjoin = on; SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_incremental_sort = off;
SELECT count(*), sum(a1) FROM p2() /* PL2 */;
-- PL3: plpgsql SELECT INTO of one aggregate row over the Q1 filter (SPI tcount 1)
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = on; SET enable_hashjoin = on; SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_incremental_sort = off;
SELECT p3() /* PL3 */;
-- PL4: plpgsql loop of 100k one-row primary-key lookups (SPI cached plan; the per-execution fixed cost, output-path.md §5)
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = on; SET enable_hashjoin = on; SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_incremental_sort = off;
SELECT p4() /* PL4 */;
-- C9: columnar source: count(*) WHERE dt LIKE 'ep%' on the dictionary text: the predicate evaluated once per dictionary entry, rows tested by code (columnar-encoded-lanes.md §4)
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = on; SET enable_hashjoin = on; SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_incremental_sort = off;
EXPLAIN (COSTS OFF) SELECT count(*) FROM gc WHERE dt LIKE 'ep%' /* C9 */;
-- C9b: columnar source: count(*) WHERE dt <> 'gamma' AND dt <> 'zeta' (the first qual by code, the second over the Datum lane)
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = on; SET enable_hashjoin = on; SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_incremental_sort = off;
EXPLAIN (COSTS OFF) SELECT count(*) FROM gc WHERE dt <> 'gamma' AND dt <> 'zeta' /* C9b */;
-- C2f: columnar source: dt, count(*) WHERE dt <> 'zeta' GROUP BY dt ORDER BY count DESC LIMIT 5 (the qual on the key by code, the key fold by code; kernel: the two-level code aggregation)
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = on; SET enable_hashjoin = on; SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_incremental_sort = off;
EXPLAIN (COSTS OFF) SELECT dt, count(*) AS c FROM gc WHERE dt <> 'zeta' GROUP BY dt ORDER BY c DESC, dt LIMIT 5 /* C2f */;
-- C2k: columnar source: dt, a, count(*) GROUP BY dt, a (a dictionary text key beside a small-domain int key, 161 groups)
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = on; SET enable_hashjoin = on; SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_incremental_sort = off;
EXPLAIN (COSTS OFF) SELECT dt, a, count(*) FROM gc GROUP BY dt, a ORDER BY dt, a /* C2k */;
-- M1: corpus M1: PlainAgg sum(n), avg(n) over a short-form numeric column (numeric_avg_accum per row)
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = on; SET enable_hashjoin = on; SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_incremental_sort = off;
EXPLAIN (COSTS OFF) SELECT sum(n), avg(n) FROM gm /* M1 */;
-- M2: corpus M2: HashAgg 23 groups, sum(n * m) (numeric_mul + numeric_avg_accum per row)
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = on; SET enable_hashjoin = on; SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_incremental_sort = off;
EXPLAIN (COSTS OFF) SELECT a, sum(n * m) FROM gm GROUP BY a ORDER BY a /* M2 */;
-- M3: corpus M3: count(*) under numeric compares and an add (numeric_gt, numeric_add, numeric_lt per row)
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = on; SET enable_hashjoin = on; SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_incremental_sort = off;
EXPLAIN (COSTS OFF) SELECT count(*) FROM gm WHERE n > 500 AND n + m < 900.5 /* M3 */;
-- M4: corpus M4: id, n * m projected at 4.3% (numeric_mul into the output row)
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = on; SET enable_hashjoin = on; SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_incremental_sort = off;
EXPLAIN (COSTS OFF) SELECT id, n * m AS v FROM gm WHERE a = 3 /* M4 */;
-- T1: corpus T1: HashAgg 24 groups keyed by date_trunc('month', ts) (timestamp_trunc per row)
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = on; SET enable_hashjoin = on; SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_incremental_sort = off;
EXPLAIN (COSTS OFF) SELECT date_trunc('month', ts) AS k, count(*) FROM gd GROUP BY 1 ORDER BY 1 /* T1 */;
-- T2: corpus T2: count(*) WHERE extract(year FROM ts) = 2024 (extract_timestamp into numeric_eq per row)
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = on; SET enable_hashjoin = on; SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_incremental_sort = off;
EXPLAIN (COSTS OFF) SELECT count(*) FROM gd WHERE extract(year FROM ts) = 2024 /* T2 */;
-- T3: corpus T3: HashAgg 7 groups keyed by extract(dow FROM d) over a date column (extract_date per row)
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = on; SET enable_hashjoin = on; SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_incremental_sort = off;
EXPLAIN (COSTS OFF) SELECT extract(dow FROM d) AS k, count(*) FROM gd GROUP BY 1 ORDER BY 1 /* T3 */;
-- MO1: corpus MO1: Memoize with 100k distinct int keys over a 100k-row indexed inner (memoize-array.md: miss-heavy, evictions and compaction under the hash memory limit)
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = on; SET enable_hashjoin = off; SET enable_nestloop = on; SET enable_mergejoin = off; SET enable_incremental_sort = off; SET enable_memoize = on;
EXPLAIN (COSTS OFF) SELECT count(*), sum(g6.v) FROM gmo JOIN g6 ON gmo.m = g6.k /* MO1 */;
-- MO2: corpus MO2: Memoize with a text key (1000 distinct) over a 1000-row text-indexed inner (a byte key in the array: memoize-array.md section 7)
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = on; SET enable_hashjoin = off; SET enable_nestloop = on; SET enable_mergejoin = off; SET enable_incremental_sort = off; SET enable_memoize = on;
EXPLAIN (COSTS OFF) SELECT count(*), sum(g7.v) FROM gmo JOIN g7 ON gmo.t = g7.t /* MO2 */;
-- P1f: floor of P1: the closed form sum(max(a % 3, 0)); no set is iterated (P1 is the calibration kernel)
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = on; SET enable_hashjoin = on; SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_incremental_sort = off;
EXPLAIN (COSTS OFF) SELECT count(*) FROM (SELECT generate_series(1, a % 3) FROM g) s /* P1f */;
-- Q22f: floor of Q22: the counter loop alone; no working table (Q22 is the calibration kernel)
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = on; SET enable_hashjoin = on; SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_incremental_sort = off;
EXPLAIN (COSTS OFF) WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM t WHERE n < 2000000) SELECT count(*) FROM t /* Q22f */;
-- S1s: attribution: S1's kernel through the SRF tuplestore (S1's kernel column is the /wire form)
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = on; SET enable_hashjoin = on; SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_incremental_sort = off;
EXPLAIN (COSTS OFF) SELECT id, b FROM g WHERE a > 21 /* S1s */;
-- S2s: attribution: S2's kernel through the SRF tuplestore
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = on; SET enable_hashjoin = on; SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_incremental_sort = off;
EXPLAIN (COSTS OFF) SELECT id, b FROM g WHERE a >= 1 /* S2s */;
-- S3s: attribution: S3's kernel through the SRF tuplestore
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = on; SET enable_hashjoin = on; SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_incremental_sort = off;
EXPLAIN (COSTS OFF) SELECT id, length(t), upper(t) FROM g WHERE a = 3 /* S3s */;
-- S4s: attribution: S4's kernel through the SRF tuplestore
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = on; SET enable_hashjoin = on; SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_incremental_sort = off;
EXPLAIN (COSTS OFF) SELECT id, c * 2.5 + b::float8 AS v FROM g WHERE f /* S4s */;
-- W1s: attribution: W1's kernel through the SRF tuplestore
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = on; SET enable_hashjoin = on; SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_incremental_sort = off;
EXPLAIN (COSTS OFF) SELECT c3, c7 FROM gw WHERE c1 > 699 /* W1s */;
-- N2s: attribution: N2's kernel through the SRF tuplestore
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = on; SET enable_hashjoin = on; SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_incremental_sort = off;
EXPLAIN (COSTS OFF) SELECT id, b FROM gn WHERE a > 15 /* N2s */;
-- J4s: attribution: J4's kernel through the SRF tuplestore
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = on; SET enable_hashjoin = on; SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_incremental_sort = off;
EXPLAIN (COSTS OFF) SELECT g.id, g2.v FROM g JOIN g2 ON g.a = g2.k WHERE g2.k > 20 /* J4s */;
-- C14: columnar source, clustered twin (gcs, rows in dt order): dt, count(*) GROUP BY dt (the codes key fold with run collapse: one record update per run; kernel: the (code, run) fold)
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = on; SET enable_hashjoin = on; SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_incremental_sort = off;
EXPLAIN (COSTS OFF) SELECT dt, count(*) FROM gcs GROUP BY dt ORDER BY dt /* C14 */;
-- C15: columnar source, clustered twin (gcs, rows in (dt, a) order): a, sum(b) GROUP BY a (the word key fold with run collapse, 161 runs of ~12k rows; kernel: the (key, run) fold with the sum in a register)
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = on; SET enable_hashjoin = on; SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_incremental_sort = off;
EXPLAIN (COSTS OFF) SELECT a, sum(b) FROM gcs GROUP BY a ORDER BY a /* C15 */;
-- C16: columnar source: a, c, count(*) GROUP BY a, c (two 23-value word keys: the word-only product front, 529 records; kernel: a dense 23 x 23 count array)
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = on; SET enable_hashjoin = on; SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_incremental_sort = off;
EXPLAIN (COSTS OFF) SELECT a, c, count(*) FROM gc GROUP BY a, c ORDER BY a, c /* C16 */;
-- C16h: the heap twin of C16 (the same product front over heap pages)
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = on; SET enable_hashjoin = on; SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_incremental_sort = off;
EXPLAIN (COSTS OFF) SELECT a, c, count(*) FROM gch GROUP BY a, c ORDER BY a, c /* C16h */;
-- SVkey: a mostly-empty text group key: notes <> '' GROUP BY notes, top 10 by count (string-views.md §7)
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = on; SET enable_hashjoin = on; SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_incremental_sort = off;
EXPLAIN (COSTS OFF) SELECT notes, count(*) AS c FROM sv WHERE notes <> '' GROUP BY 1 ORDER BY 2 DESC, 1 LIMIT 10 /* SVkey */;
-- SVlike: a substring count: count(*) WHERE media_url LIKE '%atlas%'
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = on; SET enable_hashjoin = on; SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_incremental_sort = off;
EXPLAIN (COSTS OFF) SELECT count(*) FROM sv WHERE media_url LIKE '%atlas%' /* SVlike */;
-- SVmin: min(text) per text key under a LIKE and a <> '' filter
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = on; SET enable_hashjoin = on; SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_incremental_sort = off;
EXPLAIN (COSTS OFF) SELECT notes, min(media_url), count(*) AS c FROM sv WHERE media_url LIKE '%atlas%' AND notes <> '' GROUP BY 1 ORDER BY 3 DESC, 1 LIMIT 10 /* SVmin */;
-- SVsort2: a text secondary sort key: notes <> '' ORDER BY observed_at, notes LIMIT 10
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = on; SET enable_hashjoin = on; SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_incremental_sort = off;
EXPLAIN (COSTS OFF) SELECT notes FROM sv WHERE notes <> '' ORDER BY observed_at, notes LIMIT 10 /* SVsort2 */;
-- SVlen: sum(length(text)), count(*) per int key, ordered by the mean length
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = on; SET enable_hashjoin = on; SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_incremental_sort = off;
EXPLAIN (COSTS OFF) SELECT site_id, sum(length(media_url)) AS l, count(*) AS c FROM sv WHERE media_url <> '' GROUP BY 1 ORDER BY sum(length(media_url))::float8 / count(*) DESC, 1 LIMIT 25 /* SVlen */;
-- SVre: a regexp_replace capture-group key with sum(length()) (the regexp stays hosted)
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = on; SET enable_hashjoin = on; SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_incremental_sort = off;
EXPLAIN (COSTS OFF) SELECT regexp_replace(source_url, '^https?://([^/]+)/.*$', '\1') AS k, sum(length(source_url)) AS l, count(*) AS c FROM sv WHERE source_url <> '' GROUP BY 1 ORDER BY 3 DESC, 1 LIMIT 25 /* SVre */;
-- SVlong: a long text group key, top 10 by count
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = on; SET enable_hashjoin = on; SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_incremental_sort = off;
EXPLAIN (COSTS OFF) SELECT media_url, count(*) AS c FROM sv GROUP BY 1 ORDER BY 2 DESC, 1 LIMIT 10 /* SVlong */;
-- SVfilt: a text group key under an int equality filter, top 10 by count
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = on; SET enable_hashjoin = on; SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_incremental_sort = off;
EXPLAIN (COSTS OFF) SELECT taxon_label, count(*) AS c FROM sv WHERE site_id = 62 GROUP BY 1 ORDER BY 2 DESC, 1 LIMIT 10 /* SVfilt */;
-- SVkeyc: the columnar twin of SVkey (svc USING pgrcolumnar)
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = on; SET enable_hashjoin = on; SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_incremental_sort = off;
EXPLAIN (COSTS OFF) SELECT notes, count(*) AS c FROM svc WHERE notes <> '' GROUP BY 1 ORDER BY 2 DESC, 1 LIMIT 10 /* SVkeyc */;
-- SVlikec: the columnar twin of SVlike
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = on; SET enable_hashjoin = on; SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_incremental_sort = off;
EXPLAIN (COSTS OFF) SELECT count(*) FROM svc WHERE media_url LIKE '%atlas%' /* SVlikec */;
-- SVlongc: the columnar twin of SVlong
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = on; SET enable_hashjoin = on; SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_incremental_sort = off;
EXPLAIN (COSTS OFF) SELECT media_url, count(*) AS c FROM svc GROUP BY 1 ORDER BY 2 DESC, 1 LIMIT 10 /* SVlongc */;
-- Q23: BitmapHeapScan source, Q19's range as a bitmap over g_id: one bitmap page per page_next, generated deform (sources-batched.md)
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = on; SET enable_hashjoin = on; SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_incremental_sort = off; SET enable_seqscan = off; SET enable_indexscan = off; SET enable_indexonlyscan = off; SET enable_bitmapscan = on;
EXPLAIN (COSTS OFF) SELECT id, a + 1 FROM g WHERE id BETWEEN 500000 AND 1100000 /* Q23 */;
-- Q24: IndexOnlyScan source, Q19's range as an index-only scan of g_id: the index tuples batched 64 per source_batch (sources-batched.md)
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = on; SET enable_hashjoin = on; SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_incremental_sort = off; SET enable_seqscan = off; SET enable_indexscan = off; SET enable_bitmapscan = off; SET enable_indexonlyscan = on;
EXPLAIN (COSTS OFF) SELECT id FROM g WHERE id BETWEEN 500000 AND 1100000 /* Q24 */;
-- F1: direct shims F1: one by-value builtin per row in the filter (int4gcd, tier 2: direct-shims.md)
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = on; SET enable_hashjoin = on; SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_incremental_sort = off;
EXPLAIN (COSTS OFF) SELECT count(*) FROM g WHERE gcd(a, 6) = 1 /* F1 */;
-- F2: direct shims F2: a chain of four by-value builtins per row (int4larger / int4smaller nested, tier 2)
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = on; SET enable_hashjoin = on; SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_incremental_sort = off;
EXPLAIN (COSTS OFF) SELECT sum(int4larger(int4smaller(int4larger(int4smaller(a, 20), 3), 19), 4)) FROM g /* F2 */;
-- F3: direct shims F3: a 3-argument date builtin per row (make_date) compared to a date (date_gt), tier 2
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = on; SET enable_hashjoin = on; SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_incremental_sort = off;
EXPLAIN (COSTS OFF) SELECT count(*) FROM g WHERE make_date(2000 + a % 20, a % 12 + 1, a % 28 + 1) > date '2010-06-01' /* F3 */;
-- F4: by-ref shims F4: a text argument, by-value result per row (hashtext: the view->byval family, function-tiers.md section 3)
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = on; SET enable_hashjoin = on; SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_incremental_sort = off;
EXPLAIN (COSTS OFF) SELECT count(*) FROM g WHERE hashtext(t) & 7 = 0 /* F4 */;
-- F5: by-ref shims F5: a text result into the scratch region per row (translate: the view->view family) under the open-coded length
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = on; SET enable_hashjoin = on; SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_incremental_sort = off;
EXPLAIN (COSTS OFF) SELECT sum(length(translate(t, 'row', 'ROW'))) FROM g /* F5 */;
-- F6: by-ref shims F6: a numeric result into the scratch region per row (numeric_round) under the open-coded numeric compare
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = on; SET enable_hashjoin = on; SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_incremental_sort = off;
EXPLAIN (COSTS OFF) SELECT count(*) FROM gm WHERE round(n, 1) > 500 /* F6 */;
-- F7: by-ref shims F7: an interval result (timestamp_mi) compared by an interval-argument shim (interval_gt): two by-ref families per row
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = on; SET enable_hashjoin = on; SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_incremental_sort = off;
EXPLAIN (COSTS OFF) SELECT count(*) FROM gd WHERE ts - timestamp '2024-01-01' > interval '100 days' /* F7 */;
-- J5S: corpus J5's stop arm (D113): the expression probe key on a sink-ended probe pipe, so the pipe stops mid-row and resumes
SET enable_seqscan = on; SET enable_indexscan = on; SET enable_indexonlyscan = on; SET enable_bitmapscan = off; SET enable_hashagg = on; SET enable_hashjoin = on; SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_incremental_sort = off;
EXPLAIN (COSTS OFF) SELECT g.id, gk.v FROM g JOIN gk ON g.id % 1000 = gk.k /* J5S */;

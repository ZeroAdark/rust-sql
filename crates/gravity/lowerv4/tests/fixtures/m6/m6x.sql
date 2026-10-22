-- M6-RIG's m6x set (cards/M6.md L1 "tests/m6.rs"; M6-plan §8): plans the census corpora lack, run over a fresh initdb by
-- `OUT=crates/gravity/lowerv4/tests/fixtures/m6 scripts/gravity-capture-plans.sh m6x`; every planned statement becomes
-- fixtures/m6/m6x-<nnn>.plan. Three parts: G4's does-not-apply shapes (a key or fold of a type M6 does not own, which no
-- M6 name may claim), the collation matrix (one text key under "C", the database default, libc "en_US", ICU
-- "und-x-icu" and a nondeterministic ICU collation, for GROUP BY / DISTINCT / ORDER BY / a join key / min), and
-- table-free statements (VALUES, generate_series) per M6 kind and class whose rows tests/m6.rs checks against PostgreSQL
-- 18.6 (fixtures/m6/<id>.pg, scripts/gravity-v4/m5_pg_golden.py) once the name is built. The initdb is --no-locale, so
-- the database default is C; the libc and ICU collations are initdb's imports (the box needs en_US.UTF-8, M6-plan §8).
\set VERBOSITY terse
\pset pager off
SET client_min_messages = warning;
SET max_parallel_workers_per_gather = 0;
CREATE COLLATION ci (provider = icu, locale = 'und-u-ks-level2', deterministic = false);
CREATE TABLE tt (k int PRIMARY KEY, g int NOT NULL, t text, n numeric, f float8);
INSERT INTO tt SELECT i, i % 7, (ARRAY['apple','Apple','APPLE','banana','éclair','eclair','zebra',''])[1 + i % 8], i % 11 * 1.5, i % 5 FROM generate_series(1, 1000) i;
CREATE TABLE tu (t text, x int);
INSERT INTO tu SELECT (ARRAY['apple','Banana','zebra'])[1 + i % 3], i FROM generate_series(1, 30) i;
ANALYZE tt;
ANALYZE tu;
SET enable_nestloop = off;
SET enable_mergejoin = off;
SET enable_sort = on;

-- G4: an int key, an int order, an int fold, an int DISTINCT: none of them needs an M6 kind or road
SELECT g, count(*) FROM tt GROUP BY g;
SELECT k FROM tt ORDER BY g, k LIMIT 5;
SELECT min(k), max(g), sum(g) FROM tt;
SELECT count(DISTINCT g) FROM tt;

-- the collation matrix: GROUP BY a text key
SELECT t COLLATE "C", count(*) FROM tt GROUP BY 1;
SELECT t, count(*) FROM tt GROUP BY t;
SELECT t COLLATE "en_US", count(*) FROM tt GROUP BY 1;
SELECT t COLLATE "und-x-icu", count(*) FROM tt GROUP BY 1;
SELECT t COLLATE ci, count(*) FROM tt GROUP BY 1;
-- DISTINCT
SELECT DISTINCT t COLLATE "en_US" FROM tt;
SELECT DISTINCT t COLLATE ci FROM tt;
-- ORDER BY (a full sort: the order images)
SELECT k, t FROM tt ORDER BY t COLLATE "C", k;
SELECT k, t FROM tt ORDER BY t, k;
SELECT k, t FROM tt ORDER BY t COLLATE "en_US", k;
SELECT k, t FROM tt ORDER BY t COLLATE "und-x-icu", k;
SELECT k, t FROM tt ORDER BY t COLLATE ci, k;
SELECT k, t FROM tt ORDER BY t COLLATE "und-x-icu" DESC, k;
SELECT k, t FROM tt ORDER BY t COLLATE ci NULLS FIRST, k;
-- a join key (hash join on text)
SELECT tt.k, tu.x FROM tt JOIN tu ON tt.t = tu.t;
SELECT tt.k, tu.x FROM tt JOIN tu ON tt.t = tu.t COLLATE ci;
-- min / max over text (class.text) under C and en_US
SELECT min(t COLLATE "C"), max(t COLLATE "C") FROM tt;
SELECT min(t COLLATE "en_US"), max(t COLLATE "en_US") FROM tt;

-- table-free, rows against PostgreSQL 18.6 once built: one statement per kind / class (no equal-not-identical keys
-- here: '1 mon' / '30 days', 1.0 / 1.00 output any member of the group, Decision 7, which the crate's comparator does not
-- model; the e2e files hold them; an empty string is shown beside its length: a golden row is never an empty line)
SELECT t, count(*) FROM (VALUES ('b'), ('a'), ('B'), ('a'), (NULL), ('')) v(t) GROUP BY t;
SELECT t, length(t) FROM (VALUES ('b'), ('a'), ('B'), ('ab'), (NULL), ('')) v(t) ORDER BY t COLLATE "C" NULLS FIRST;
SELECT t, length(t) FROM (VALUES ('b'), ('a'), ('B'), ('ab'), ('Ab'), ('')) v(t) ORDER BY t COLLATE "en_US", t COLLATE "C";
SELECT (g % 13)::text AS k, count(*), sum(g) FROM generate_series(1, 2000) g GROUP BY 1;
SELECT u, count(*) FROM (VALUES ('a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11'::uuid), ('00000000-0000-0000-0000-000000000000'), ('a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11')) v(u) GROUP BY u;
SELECT i, count(*) FROM (VALUES ('1 mon'::interval), ('2 days'), ('1 day'), ('1 mon'), ('3 hours')) v(i) GROUP BY i ORDER BY i;
SELECT n, count(*) FROM (VALUES (1.5::numeric), (2.25), (1.5), ('NaN'), (0), ('NaN')) v(n) GROUP BY n ORDER BY n;
SELECT a, count(*) FROM (VALUES (ARRAY[1, 2]), (ARRAY[1, 2]), (ARRAY[2]), ('{}'::int[])) v(a) GROUP BY a ORDER BY a;
SELECT min(t), max(t) FROM (VALUES ('b'), ('a'), ('B')) v(t);
SELECT sum(n), avg(n) FROM (VALUES (1.5::numeric), (2.25), (NULL), (1e20)) v(n);
SELECT variance(x), stddev_samp(x), var_pop(x) FROM generate_series(1, 100) x;
SELECT sum(i), avg(i) FROM (VALUES ('1 day'::interval), ('3 hours'), ('1 mon')) v(i);
SELECT string_agg(t, ',' ORDER BY t), array_agg(t ORDER BY t) FROM (VALUES ('b'), ('a'), ('c')) v(t);
SELECT percentile_cont(0.5) WITHIN GROUP (ORDER BY x), mode() WITHIN GROUP (ORDER BY x % 3) FROM generate_series(1, 99) x;
SELECT x % 3 AS g, count(DISTINCT x % 5), count(DISTINCT x % 7) FROM generate_series(1, 300) x GROUP BY 1;
-- numeric keys and orders at a decided scale (rep.packed.hash / .order: no census fixture needs them)
SELECT x % 5 AS g, sum((x % 7) * 1.25) AS s FROM generate_series(1, 100) x GROUP BY 1 ORDER BY 2 DESC, 1 LIMIT 3;
SELECT (x % 4) * 0.25 AS k, count(*) FROM generate_series(1, 100) x GROUP BY 1;
SELECT s, count(*) FROM (SELECT x % 5 AS g, sum((x % 3) * 0.5) AS s FROM generate_series(1, 100) x GROUP BY 1) q GROUP BY s;
SELECT g, sum(n) FROM tt GROUP BY g ORDER BY 2 DESC, 1 LIMIT 3;
-- a pgrc numeric(9, 2) lane (Rep::PackedNum, decompose sources.rs): the key and order images at the column's scale; a pgrc
-- text lane (Rep::Code): the code top-k roads (code.cut / .ties / .heads: a text key ordered by a timestamp or by itself, LIMIT 10)
CREATE ACCESS METHOD pgrcolumnar TYPE TABLE HANDLER heap_tableam_handler;
CREATE TABLE tp (g int NOT NULL, p numeric(9, 2), s text) USING pgrcolumnar;
INSERT INTO tp SELECT i, (i % 13) * 1.25, (ARRAY['apple', 'banana', '', 'zebra', 'kiwi'])[1 + i % 5] FROM generate_series(1, 500) i;
ANALYZE tp;
SELECT p, count(*) FROM tp GROUP BY p;
SELECT g, p FROM tp ORDER BY p DESC, g LIMIT 5;
SELECT s FROM tp WHERE s <> '' ORDER BY s LIMIT 3;
SELECT s FROM tp WHERE s <> '' ORDER BY g DESC, s LIMIT 3;
SELECT s, g FROM tp ORDER BY s DESC, g LIMIT 4;
-- D1071 (V4R-379, main 11:49Z): a sorted group's DISTINCT aggregate over a VOLATILE argument streams one group at a time
-- (keyed/stream.rs), so a LIMIT above stops the argument where PostgreSQL's does; its per-group distinct set is M6's
-- (runs.group_distinct, D1120)
CREATE SEQUENCE m6x_s;
SET enable_hashagg = off;
SELECT x % 3 AS g, count(DISTINCT nextval('m6x_s') % 5) FROM generate_series(1, 300) x GROUP BY 1 ORDER BY 1 LIMIT 1;
RESET enable_hashagg;
-- M6-REPS HO (V4R-386): a full sort over a key with no native order image, in its type's btree comparator's order
-- (rep.hosted.order: an unconstrained numeric, an int[]); a macaddr key orders by its exact word
-- (rep.hosted.order.fixed, M6-FIXED V4R-442; rig_gaps.d: the crate does not render a macaddr)
SELECT n FROM (VALUES (1.5::numeric), (2.25), (-1), ('NaN'), (0), (1e20), (NULL)) v(n) ORDER BY n;
SELECT a FROM (VALUES (ARRAY[1, 2]), (ARRAY[2]), ('{}'::int[]), (ARRAY[1, NULL]), (NULL)) v(a) ORDER BY a DESC;
SELECT m FROM (VALUES ('08:00:2b:01:02:03'::macaddr), ('08:00:2b:01:02:01'::macaddr), (NULL)) v(m) ORDER BY m;
-- M6-HOSTED (D1326): an integral extract key over a set-returning function's timestamps: decompose keys the field's integer
-- while the cell is extract's numeric result; rep.hosted.hash converts that numeric to the key's word (D1326)
SELECT extract(year FROM t) AS y, count(*) FROM generate_series(timestamp '2020-01-01', timestamp '2024-12-31', interval '45 days') t GROUP BY 1;
-- D1272's witness (core_d1272_home_open_raise.rs; lane mdistinct): two DISTINCT arguments grouped that stay host-sorted, a
-- keyed hosted table: a sum(DISTINCT) beside the count(DISTINCT) (decompose multi_grouped keys count(DISTINCT) alone; the
-- witness was m6x-039, which the tagged pair table now serves)
SELECT x % 3 AS g, count(DISTINCT x % 5), sum(DISTINCT x % 7) FROM generate_series(1, 300) x GROUP BY 1;

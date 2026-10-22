-- M7-RIG's m7x set (cards/M7.md L1 "tests/m7.rs"; M7-plan §8): plans the census corpora lack, run over a fresh initdb by
-- `OUT=crates/gravity/lowerv4/tests/fixtures/m7 scripts/gravity-capture-plans.sh m7x`; every planned statement becomes
-- fixtures/m7/m7x-<nnn>.plan. Two parts: G4's does-not-apply shapes (an INNER hash join, NOT IN, an IN the planner
-- makes an inner join over a unique inner: no M7 kind or road may claim them), and table-free statements (VALUES,
-- generate_series) per M7 join type whose rows tests/m7.rs checks against PostgreSQL 18.6 (fixtures/m7/<id>.pg,
-- scripts/gravity-v4/m5_pg_golden.py) once the kind is built: NULL keys on either side, duplicate build keys, an empty
-- build or outer, a joinqual beyond the keys, a chain, a drain under a fold. Nested loops and merge joins are off so
-- each join is a hash join of the join type its comment names (tests/m7.rs checks the plan's jointype).
\set VERBOSITY terse
\pset pager off
SET client_min_messages = warning;
SET max_parallel_workers_per_gather = 0;
CREATE TABLE ta (k int PRIMARY KEY, g int NOT NULL, v int);
INSERT INTO ta SELECT i, i % 7, CASE WHEN i % 10 = 0 THEN NULL ELSE i % 60 END FROM generate_series(1, 1000) i;
CREATE TABLE tb (k int, w int NOT NULL);
INSERT INTO tb SELECT CASE WHEN i % 9 = 0 THEN NULL ELSE i % 50 END, i FROM generate_series(1, 200) i;
CREATE TABLE tu (k int PRIMARY KEY, w int NOT NULL);
INSERT INTO tu SELECT i, i * 3 FROM generate_series(1, 40) i;
ANALYZE ta;
ANALYZE tb;
ANALYZE tu;
SET enable_nestloop = off;
SET enable_mergejoin = off;

-- G4: an INNER hash join (M1's probe), NOT IN (a hashed SubPlan: M8's, never an anti join in PostgreSQL 18.6), an IN over a
-- unique inner (the planner's inner join): none of them needs an M7 kind or road
SELECT ta.k, tb.w FROM ta JOIN tb ON ta.v = tb.k;
SELECT k FROM ta WHERE v NOT IN (SELECT k FROM tb WHERE k IS NOT NULL);
SELECT ta.k FROM ta WHERE ta.v IN (SELECT tu.k FROM tu);
SELECT ta.g, count(*) FROM ta JOIN tu ON ta.v = tu.k GROUP BY ta.g;

-- table-free, rows against PostgreSQL 18.6 once built (x: the outer / preserved side with NULL keys and duplicates; the
-- build side's y carries NULL keys and duplicate keys too)
-- LEFT (Hash Left Join: the VALUES build, the series probe)
SELECT a.x, b.y, b.z FROM (SELECT CASE WHEN g % 5 = 0 THEN NULL ELSE g % 8 END AS x FROM generate_series(1, 40) g) a LEFT JOIN (VALUES (2, 20), (2, 21), (3, 30), (NULL, 0), (99, 990)) b(y, z) ON a.x = b.y;
-- LEFT with a joinqual beyond the key (a build column and a two-sided expression)
SELECT a.x, b.y, b.z FROM (SELECT g % 6 AS x FROM generate_series(1, 30) g) a LEFT JOIN (VALUES (1, 10), (1, 20), (2, 5), (4, 40), (NULL, 1)) b(y, z) ON a.x = b.y AND b.z > 7 AND a.x + b.z <> 21;
-- LEFT over an empty build
SELECT a.x, b.y FROM (SELECT g AS x FROM generate_series(1, 5) g) a LEFT JOIN (SELECT g AS y FROM generate_series(1, 0) g) b ON a.x = b.y;
-- SEMI (EXISTS: stop at the first match; a duplicate build key never duplicates the outer row)
SELECT a.x FROM (SELECT CASE WHEN g % 7 = 0 THEN NULL ELSE g % 9 END AS x FROM generate_series(1, 50) g) a WHERE EXISTS (SELECT 1 FROM (VALUES (2), (2), (3), (NULL), (5)) b(y) WHERE b.y = a.x);
-- ANTI (NOT EXISTS: a NULL outer key has no match, so it is emitted)
SELECT a.x FROM (SELECT CASE WHEN g % 7 = 0 THEN NULL ELSE g % 9 END AS x FROM generate_series(1, 50) g) a WHERE NOT EXISTS (SELECT 1 FROM (VALUES (2), (2), (3), (NULL), (5)) b(y) WHERE b.y = a.x);
-- ANTI over an empty build: every outer row
SELECT a.x FROM (SELECT g AS x FROM generate_series(1, 6) g) a WHERE NOT EXISTS (SELECT 1 FROM generate_series(1, 0) b(y) WHERE b.y = a.x);
-- RIGHT (Hash Right Join: the small preserved side is the build; its unmatched rows drain, NULL keys included)
SELECT a.x, s.g FROM (VALUES (2), (2), (3), (NULL), (999), (-1)) a(x) LEFT JOIN generate_series(1, 300) s(g) ON a.x = s.g % 100;
-- FULL (Hash Full Join: the left fill and the right drain, NULL keys on both sides)
SELECT a.x, b.y FROM (VALUES (1), (2), (2), (NULL), (7)) a(x) FULL JOIN (VALUES (2), (3), (3), (NULL), (8)) b(y) ON a.x = b.y;
-- FULL with a joinqual beyond the key (a matched key the joinqual rejects stays unmatched on both sides)
SELECT a.x, a.p, b.y, b.q FROM (VALUES (1, 1), (2, 5), (2, 6), (NULL, 0)) a(x, p) FULL JOIN (VALUES (2, 5), (1, 9), (3, 3), (NULL, 0)) b(y, q) ON a.x = b.y AND a.p = b.q;
-- FULL over an empty outer: the whole build drains
SELECT a.x, b.y FROM (SELECT g AS x FROM generate_series(1, 0) g) a FULL JOIN (VALUES (1), (NULL), (1)) b(y) ON a.x = b.y;
-- RIGHT SEMI (Hash Right Semi Join: the small outer is the build, each build row out once; no unique-ified inner)
SET enable_hashagg = off;
SET enable_sort = off;
SELECT a.x FROM (VALUES (2), (2), (3), (NULL), (999)) a(x) WHERE a.x IN (SELECT s.g % 50 FROM generate_series(1, 2000) s(g));
RESET enable_hashagg;
RESET enable_sort;
-- RIGHT ANTI (Hash Right Anti Join: the build rows no probe row matched, the NULL key among them)
SELECT a.x FROM (VALUES (2), (2), (3), (NULL), (999)) a(x) WHERE NOT EXISTS (SELECT 1 FROM generate_series(1, 2000) s(g) WHERE s.g % 50 = a.x);
-- the drain under a fold and a keyed fold (a sink end)
SELECT count(*), count(a.x), count(b.y), sum(b.y) FROM (VALUES (1), (2), (2), (NULL)) a(x) FULL JOIN (VALUES (2), (3), (NULL)) b(y) ON a.x = b.y;
SELECT b.y IS NULL AS unmatched, count(*) FROM (VALUES (1), (2), (2), (NULL)) a(x) FULL JOIN (VALUES (2), (3), (NULL)) b(y) ON a.x = b.y GROUP BY 1;
-- a chain: LEFT, then INNER on the filled column, then LEFT again (three probes on one outer pipeline)
SELECT a.x, b.y, c.w, d.v FROM (SELECT g % 10 AS x FROM generate_series(1, 30) g) a LEFT JOIN (VALUES (1, 1), (2, 2), (2, 2), (3, NULL)) b(y, m) ON a.x = b.y JOIN (VALUES (1, 100), (2, 200), (2, 201)) c(n, w) ON b.m = c.n LEFT JOIN (VALUES (1, 7), (5, 8)) d(u, v) ON c.n = d.u;

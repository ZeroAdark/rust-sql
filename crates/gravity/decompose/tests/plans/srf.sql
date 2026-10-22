-- The set-returning-function set (docs/gravity/decompose.md §2.13: ProjectSet / FunctionScan → the SRF loop desc),
-- run over a fresh initdb by scripts/gravity-capture-plans.sh srf; every planned statement becomes srf-<nnn>.plan.
\set VERBOSITY terse
\pset pager off
SET client_min_messages = warning;
SET max_parallel_workers_per_gather = 0;
CREATE TABLE t (a int PRIMARY KEY, b int NOT NULL, c text, arr int[]);
INSERT INTO t SELECT i, i % 7 + 1, 'c' || i || ',x' || i, ARRAY[i, i + 1] FROM generate_series(1, 500) i;
CREATE FUNCTION three() RETURNS SETOF int LANGUAGE sql AS 'SELECT 1 UNION ALL SELECT 2 UNION ALL SELECT 3';
ANALYZE t;

-- ProjectSet: SRFs in the target list
SELECT generate_series(1, 3);
SELECT generate_series(1, 3), generate_series(1, 2);
SELECT a, generate_series(1, b) FROM t WHERE a < 4;
SELECT generate_series(1, b) * 2 AS g FROM t WHERE a < 4;
SELECT a, unnest(arr) FROM t WHERE a < 4;
SELECT a, regexp_matches(c, '(\d+)', 'g') FROM t WHERE a < 4;
SELECT a, unnest(arr), generate_series(1, b) FROM t WHERE a < 3;
SELECT three(), a FROM t WHERE a < 3;
SELECT jsonb_array_elements('[1, 2]'::jsonb);
SELECT generate_series(1, 5) FROM t WHERE a = 1 LIMIT 3;
SELECT generate_series(1, 5) OFFSET 2;

-- FunctionScan: SRFs in FROM
SELECT * FROM generate_series(1, 5) g(i) WHERE i > 2;
SELECT g, n FROM generate_series(10, 20) WITH ORDINALITY g(g, n) WHERE g > 13;
SELECT * FROM unnest(ARRAY[1, 2, 3]) u(v);
SELECT * FROM unnest(ARRAY[1, 2, 3], ARRAY['a', 'b']) u(v, s);
SELECT * FROM ROWS FROM (generate_series(1, 3), unnest(ARRAY['a', 'b'])) AS r(i, s);
SELECT * FROM regexp_matches('abcabc', 'b', 'g') m;
SELECT * FROM regexp_split_to_table('a,b,c', ',') s;
SELECT * FROM string_to_table('a b c', ' ') s;
SELECT * FROM jsonb_each('{"k": 1, "j": 2}'::jsonb);
SELECT * FROM three() f(i) WHERE i <> 2;
SELECT * FROM generate_series(timestamp '2024-01-01', timestamp '2024-01-03', interval '1 day') d;
SELECT t.a, g FROM t, LATERAL generate_series(1, t.b) g WHERE t.a < 3;
SELECT i FROM generate_series(1, 100) g(i) ORDER BY i DESC LIMIT 3;
SELECT i FROM generate_series(1, 100) g(i) WHERE i % 10 = 0 ORDER BY i;
SELECT * FROM now() n;
SELECT * FROM generate_series(1, 3) g(i), generate_series(1, 2) h(j) WHERE i <> j;
SELECT * FROM (SELECT generate_series(1, 3) AS g) s WHERE g > 1;

-- M8-RERUN R3 (V4R-439, store_reopen): e2e/gravity/v4/m8_store_reopen.sql's LATERAL aggregate over a VOLATILE set function
-- called with the outer row's value (a FunctionScan store re-opened per outer row with its exec param)
CREATE TABLE sro (id int, k int);
INSERT INTO sro SELECT i, i % 9 FROM generate_series(1, 3) i;
CREATE SEQUENCE sr_calls;
CREATE FUNCTION sr_f(n int) RETURNS SETOF int LANGUAGE plpgsql VOLATILE AS $$
BEGIN
  PERFORM nextval('sr_calls');
  RETURN QUERY SELECT g FROM generate_series(1, coalesce(n, 0)) g;
END $$;
ANALYZE sro;
SET enable_hashjoin = off;
SET enable_mergejoin = off;
SET enable_memoize = off;
SET enable_material = off;
SELECT o.id, a.c, a.s FROM sro o, LATERAL (SELECT count(*) AS c, sum(x) AS s FROM sr_f(o.k) x) a ORDER BY o.id;
SELECT sum(a.c * o.id), sum(a.s) FROM sro o, LATERAL (SELECT count(*) AS c, sum(x) AS s FROM sr_f(o.k) x) a;

-- Multi-batch hash joins build skew buckets by hashing the outer key's MCVs;
-- array and composite hash functions need an armed result context for that.
CREATE TYPE wio393_pair AS (x int, y text);
CREATE TABLE wio393 (id int, k int[], p wio393_pair, t text, n numeric);
INSERT INTO wio393 SELECT i, ARRAY[i % 10], ROW(i % 10, 'a')::wio393_pair, 't' || (i % 10), (i % 10) * 1.5 FROM generate_series(1, 2000) i;
ANALYZE wio393;
SET enable_mergejoin = off;
SET enable_nestloop = off;
SET work_mem = '64kB';
EXPLAIN (COSTS OFF) SELECT count(*) FROM wio393 x JOIN wio393 y ON x.k = y.k;
SELECT count(*) FROM wio393 x JOIN wio393 y ON x.k = y.k;
SELECT count(*) FROM wio393 x JOIN wio393 y ON x.p = y.p;
SELECT count(*) FROM wio393 x JOIN wio393 y ON x.t = y.t;
SELECT count(*) FROM wio393 x JOIN wio393 y ON x.n = y.n;
SELECT count(*) FROM wio393 x LEFT JOIN wio393 y ON x.k = y.k AND y.id > 1000;
RESET work_mem;
SELECT count(*) FROM wio393 x JOIN wio393 y ON x.k = y.k;

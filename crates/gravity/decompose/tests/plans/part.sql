-- The partitioned Append / MergeAppend set (docs/gravity/decompose.md §2.11: Append, MergeAppend, PartitionPruneInfo),
-- run over a fresh initdb by scripts/gravity-capture-plans.sh part; every planned statement becomes part-<nnn>.plan.
\set VERBOSITY terse
\pset pager off
SET client_min_messages = warning;
SET max_parallel_workers_per_gather = 0;
CREATE TABLE part (k int NOT NULL, v int, s text) PARTITION BY RANGE (k);
CREATE TABLE part1 PARTITION OF part FOR VALUES FROM (0) TO (100);
CREATE TABLE part2 PARTITION OF part FOR VALUES FROM (100) TO (200);
CREATE TABLE part3 PARTITION OF part FOR VALUES FROM (200) TO (300);
CREATE INDEX part_k ON part (k);
CREATE TABLE hp (h int NOT NULL, v int) PARTITION BY HASH (h);
CREATE TABLE hp0 PARTITION OF hp FOR VALUES WITH (MODULUS 2, REMAINDER 0);
CREATE TABLE hp1 PARTITION OF hp FOR VALUES WITH (MODULUS 2, REMAINDER 1);
CREATE TABLE u (x int PRIMARY KEY, y int);
CREATE TABLE a1 (k int, v int);
CREATE TABLE a2 (k int, v int);
INSERT INTO part SELECT i % 300, i, 's' || i FROM generate_series(1, 6000) i;
INSERT INTO hp SELECT i % 1000, i FROM generate_series(1, 4000) i;
INSERT INTO u SELECT i, i % 300 FROM generate_series(1, 400) i;
INSERT INTO a1 SELECT i, i FROM generate_series(1, 100) i;
INSERT INTO a2 SELECT i, -i FROM generate_series(1, 100) i;
ANALYZE part; ANALYZE hp; ANALYZE u; ANALYZE a1; ANALYZE a2;

-- Append over partitions: no pruning, static pruning to two / one partition, a filter on a non-key column
SELECT k, v FROM part WHERE v > 5990;
SELECT k, v FROM part WHERE k < 150 AND v > 5990;
SELECT k, v FROM part WHERE k = 250 AND v > 5990;
SELECT k FROM part WHERE k >= 100 AND k < 250 AND s = 's42';
SELECT h, v FROM hp WHERE h = 7;
SELECT h, v FROM hp WHERE v = 7;

-- Append of plain relations (UNION ALL)
SELECT k, v FROM a1 UNION ALL SELECT k, v FROM a2;
SELECT k FROM a1 WHERE v > 90 UNION ALL SELECT k FROM a2 WHERE v > -10;
SELECT * FROM (SELECT k, v FROM a1 UNION ALL SELECT k, v FROM a2) s WHERE s.v <> 5 ORDER BY s.k LIMIT 3;

-- MergeAppend (an ordered walk over the partitions' indexes) and Append under a Limit
SET enable_seqscan = off;
SELECT k, v FROM part ORDER BY k LIMIT 5;
SELECT k, v FROM part WHERE k > 50 ORDER BY k DESC LIMIT 5;
SELECT k, v FROM part WHERE k >= 100 ORDER BY k LIMIT 5;
SELECT k FROM part WHERE k = 42 ORDER BY k;
RESET enable_seqscan;
SELECT k, v FROM part ORDER BY k LIMIT 5;
SELECT k, v FROM a1 UNION ALL SELECT k, v FROM a2 ORDER BY k LIMIT 5;
SELECT k, v FROM part LIMIT 5;

-- run-time pruning: initial (a generic plan's extern parameter) and per-rescan (an exec parameter under a nested loop)
PREPARE pp (int) AS SELECT v FROM part WHERE k = $1;
SET plan_cache_mode = force_generic_plan;
EXECUTE pp(5);
RESET plan_cache_mode;
SET enable_hashjoin = off; SET enable_mergejoin = off;
SELECT u.x, part.v FROM u JOIN part ON part.k = u.y WHERE u.x < 3;
RESET enable_hashjoin; RESET enable_mergejoin;

-- writes through the partition root (routing) and on one partition
INSERT INTO part VALUES (5, 1, 'x');
INSERT INTO part1 VALUES (6, 1, 'y');
UPDATE part SET v = v + 1 WHERE k = 5 AND s = 'x';
UPDATE part1 SET v = v + 1 WHERE k = 6;
DELETE FROM part WHERE k = 5 AND s = 'x';
DELETE FROM part1 WHERE k = 6;

-- MergeAppend proper: an order the partition bounds do not give (a hash key, a non-key column, sorted UNION ALL arms)
CREATE INDEX hp_h ON hp (h);
CREATE INDEX part_v ON part (v);
SET enable_seqscan = off;
SELECT h, v FROM hp ORDER BY h LIMIT 5;
SELECT k, v FROM part ORDER BY v LIMIT 5;
SELECT k, v FROM part WHERE v > 100 ORDER BY v DESC LIMIT 5;
RESET enable_seqscan;
SELECT h, v FROM hp ORDER BY h LIMIT 5;
(SELECT k FROM a1 ORDER BY k) UNION ALL (SELECT k FROM a2 ORDER BY k) ORDER BY k LIMIT 3;
SET enable_hashagg = off;
SELECT k FROM a1 UNION SELECT k FROM a2 ORDER BY k LIMIT 3;
RESET enable_hashagg;

-- initial pruning (a generic plan's extern parameter) over index / sequential children
SET enable_bitmapscan = off;
EXECUTE pp(5);
PREPARE pq (int) AS SELECT k, v FROM part WHERE k < $1 AND v > 5990;
SET plan_cache_mode = force_generic_plan;
EXECUTE pq(150);
PREPARE pr (int) AS SELECT k, v FROM part WHERE k >= $1 ORDER BY k LIMIT 3;
EXECUTE pr(150);
RESET plan_cache_mode;
RESET enable_bitmapscan;

-- per-rescan pruning (an exec parameter under a nested loop) with bitmap scans off: the typed refusal
SET enable_bitmapscan = off; SET enable_hashjoin = off; SET enable_mergejoin = off;
SELECT u.x, part.v FROM u JOIN part ON part.k = u.y WHERE u.x < 3;
RESET enable_bitmapscan; RESET enable_hashjoin; RESET enable_mergejoin;

-- a btree-probe pruned UPDATE / DELETE through the parent (write_route: the leaf's own index probe, no bitmap)
SET enable_bitmapscan = off; SET enable_seqscan = off;
UPDATE part SET v = v + 1 WHERE k = 7 AND s = 's7';
DELETE FROM part WHERE k = 7 AND s = 's7';
RESET enable_bitmapscan; RESET enable_seqscan;

-- several result relations streamed into the Append's arms (V4R-214, M4-ROUTE R2): a DELETE through a list root not
-- pruned to one leaf, one leaf attached in another column order, RETURNING per arm from its own target's list
-- (returningLists[i], that leaf's attnos); an inheritance DELETE whose child has extra columns, RETURNING
CREATE TABLE pret (a int, b float8, c text) PARTITION BY LIST (a);
CREATE TABLE pret1 PARTITION OF pret FOR VALUES IN (1);
CREATE TABLE pret2 PARTITION OF pret FOR VALUES IN (2);
CREATE TABLE pret3 (c text, a int, b float8);
ALTER TABLE pret ATTACH PARTITION pret3 FOR VALUES IN (3);
DELETE FROM pret WHERE b > 0 RETURNING *;
CREATE TABLE iret (k int, v text);
CREATE TABLE iretc (w text, extra int) INHERITS (iret);
DELETE FROM iret WHERE k > 0 RETURNING *;

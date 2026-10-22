# NEW-43 / D2910 (lane effects): INCSTREAM (lowerv4 ops/sort/inc_stream.rs, a LIMIT over LockRows over an Incremental Sort
# over an index scan, the sort producer pulled by the lock pipeline) whose second pull meets a prefix group past the
# stream's in-memory run. The first pull closes the three one-row groups (the LIMIT's count); SKIP LOCKED passes over ids
# 1 and 3 (s1 holds them) and locks id 2, so the reader pulls again; the producer then fills its run inside the
# 300,000-row group k = 10, falls back to the breaker form (its runs put, read to the end) and the reader reads the host
# merge PAST the three rows it already handed to the lock: they are all in closed prefix groups, so they are exactly the
# merge's first three rows. Read from the merge's first row, id 2 (already locked by s2) would come back a second time
# and the OFFSET would count it twice. PG returns 2, 4, 5 and, past 299,999 rows, the big group's last row and two more.

setup
{
 CREATE TABLE qj (id int NOT NULL, k int NOT NULL, note text);
 INSERT INTO qj SELECT i, CASE WHEN i <= 3 THEN i WHEN i <= 300003 THEN 10 ELSE 11 + (i - 300004) / 2 END, 'n' || (i % 7) FROM generate_series(1, 340000) i;
 CREATE INDEX qj_k ON qj (k);
 CREATE UNIQUE INDEX qj_id ON qj (id);
 ALTER TABLE qj ALTER k SET STATISTICS 10000, ALTER id SET STATISTICS 10000;
 ANALYZE qj;
}

teardown
{
 DROP TABLE qj;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1h	{ SELECT id FROM qj WHERE id IN (1, 3) ORDER BY id FOR UPDATE; }
step s1c	{ COMMIT; }

session s2
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; SET enable_seqscan = off; SET enable_bitmapscan = off; SET max_parallel_workers_per_gather = 0; }
step s2e	{ EXPLAIN (COSTS OFF) SELECT id, k FROM qj ORDER BY k, id LIMIT 3 FOR UPDATE SKIP LOCKED; }
step s2k	{ SELECT id, k, note FROM qj ORDER BY k, id LIMIT 3 FOR UPDATE SKIP LOCKED; }
step s2o	{ SELECT id, k FROM qj ORDER BY k, id LIMIT 3 OFFSET 299999 FOR UPDATE SKIP LOCKED; }
step s2c	{ COMMIT; }

permutation s2e s1c s2c
permutation s1h s2k s1c s2c
permutation s1h s2o s1c s2c

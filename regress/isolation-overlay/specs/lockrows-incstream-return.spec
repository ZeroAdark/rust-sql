# NEW-43 A2 (lane effects): INCSTREAM (lowerv4 ops/sort/inc_stream.rs, a LIMIT over LockRows over an Incremental Sort over
# an index scan, the sort producer pulled by the lock pipeline) back to streaming after a prefix group past its in-memory
# run. The 250,000-row group k = 1 fills the run: the producer falls back, puts its runs at the group's close and stops
# at the first row of k = 2; the reader reads the runs' merge past the rows it handed on, re-arms the region and pulls
# on, so the producer streams from that row again. s1 holds id 5 (inside the big group) and ids 250001 and 250003 (the
# small groups after it): SKIP LOCKED passes over them, so the OFFSET counts past id 5 and the reader pulls again after
# the return. Read twice or dropped at the return, the row that opened k = 2 would shift or repeat the rows. PG returns
# 250000, 250002, 250004 and 250005.

setup
{
 CREATE TABLE qr (id int NOT NULL, k int NOT NULL, note text);
 INSERT INTO qr SELECT i, CASE WHEN i <= 250000 THEN 1 ELSE 2 + (i - 250001) / 2 END, 'n' || (i % 7) FROM generate_series(1, 260000) i;
 CREATE INDEX qr_k ON qr (k);
 CREATE UNIQUE INDEX qr_id ON qr (id);
 ALTER TABLE qr ALTER k SET STATISTICS 10000, ALTER id SET STATISTICS 10000;
 ANALYZE qr;
}

teardown
{
 DROP TABLE qr;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1h	{ SELECT id FROM qr WHERE id IN (5, 250001, 250003) ORDER BY id FOR UPDATE; }
step s1c	{ COMMIT; }

session s2
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; SET enable_seqscan = off; SET enable_bitmapscan = off; SET max_parallel_workers_per_gather = 0; }
step s2e	{ EXPLAIN (COSTS OFF) SELECT id, k FROM qr ORDER BY k, id LIMIT 4 OFFSET 249998 FOR UPDATE SKIP LOCKED; }
step s2a	{ SELECT id, k FROM qr ORDER BY k, id LIMIT 4 OFFSET 249998 FOR UPDATE SKIP LOCKED; }
step s2b	{ SELECT id, k, note FROM qr ORDER BY k, id LIMIT 5 OFFSET 249997 FOR UPDATE SKIP LOCKED; }
step s2c	{ COMMIT; }

permutation s2e s1c s2c
permutation s1h s2a s1c s2c
permutation s1h s2b s1c s2c

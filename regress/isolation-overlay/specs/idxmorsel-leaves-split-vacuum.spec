# idxmorsel A2 entry 2: a btree index scan served as the bitmap morsel form, its leaves walked by several pool threads
# that seize leaf pages of one shared parallel btree scan (each through its own view of the leader's frozen index
# image), while another session changes the index under the reader's snapshot: leaf page splits from keys inserted
# between every existing pair, and a VACUUM that removes dead entries and deletes the leaf pages they emptied. The
# ranges are large enough for several threads at W8 (one per 16384 estimated rows). The reader's rows must equal a
# serial scan's under the same snapshot, at W0 and at W8, before and after it commits.
#   (1) dead rows removed and their leaf pages deleted, then splits everywhere in the range, under one snapshot
#   (2) splits while the snapshot is open, VACUUM after the inserter aborts (dead entries in split pages)
#   (3) the scan between the VACUUM and the inserts, then after both (the snapshot sees neither)
# Expected output is PostgreSQL 18.6's (the C oracle).
setup
{
 CREATE TABLE lsv (id int NOT NULL, k int NOT NULL, pad text) WITH (fillfactor = 90, autovacuum_enabled = off);
 INSERT INTO lsv SELECT i, i * 2, 'p' || i FROM generate_series(1, 150000) i;
 CREATE INDEX lsv_k ON lsv (k);
 ANALYZE lsv;
}

teardown
{
 DROP TABLE lsv;
}

session s1
setup		{ SET enable_seqscan = off; SET enable_bitmapscan = off; SET enable_indexonlyscan = off; }
step s1b	{ BEGIN ISOLATION LEVEL REPEATABLE READ; SELECT count(*) FROM lsv WHERE k < 0; }
step s1w0	{ SET max_parallel_workers_per_gather = 0; SELECT count(*), sum(k::bigint), sum(id::bigint), min(k), max(k) FROM lsv WHERE k BETWEEN 1000 AND 280000; }
step s1w8	{ SET max_parallel_workers_per_gather = 8; SELECT count(*), sum(k::bigint), sum(id::bigint), min(k), max(k) FROM lsv WHERE k BETWEEN 1000 AND 280000; }
step s1any	{ SET max_parallel_workers_per_gather = 8; SELECT count(*), sum(k::bigint) FROM lsv WHERE k BETWEEN 90000 AND 290000 AND k % 4 = 2; }
step s1c	{ COMMIT; }

session s2
step s2del	{ DELETE FROM lsv WHERE k BETWEEN 40000 AND 100000 OR k % 6 = 0; }
step s2ins	{ INSERT INTO lsv SELECT 1000000 + i, i * 2 + 1, 'q' || i FROM generate_series(400, 140000) i; }
step s2bi	{ BEGIN; INSERT INTO lsv SELECT 2000000 + i, i * 2 + 1, 'r' || i FROM generate_series(400, 140000) i; }
step s2a	{ ROLLBACK; }

session s3
step s3vac	{ VACUUM lsv; }

permutation s2del s1b s3vac s2ins s1w0 s1w8 s1any s1c s1w0 s1w8 s1any
permutation s1b s2bi s1w0 s2a s3vac s1w8 s1any s1c s1w0 s1w8
permutation s2del s1b s1w0 s3vac s1w8 s2ins s1w0 s1w8 s1c s1w8

# idxmorsel I2: an ordered btree index scan read by the pool in key ranges (the ranges cut at the index's separators on the
# leader, each claim a range one pool thread reads forward through its own view of the leader's frozen index image, the
# claims' rows handed on in claim order), while another session changes the index under the reader's snapshot: leaf page
# splits from keys inserted between every existing pair, and a VACUUM that removes dead entries and deletes the leaf pages
# they emptied. The range stays large enough for several threads at W8 after the VACUUM's new row count. The reader's
# rows, in index order, must equal a serial scan's under the same snapshot, at W0 and at W8, before and after it commits.
# The width is set by its own step (a statement after a SET in one query string is pulled on the leader alone), and the
# W8 statement's text is its own (a comment), so its plan is decomposed in a session with workers.
#   (1) dead rows removed and their leaf pages deleted, then splits everywhere in the range, under one snapshot
#   (2) splits while the snapshot is open, VACUUM after the inserter aborts (dead entries in split pages)
#   (3) the scan between the VACUUM and the inserts, then after both (the snapshot sees neither)
# Expected output is PostgreSQL 18.6's (the C oracle).
setup
{
 CREATE TABLE rsv (id int NOT NULL, k int NOT NULL, pad text) WITH (fillfactor = 90, autovacuum_enabled = off);
 INSERT INTO rsv SELECT i, i * 2, 'p' || i FROM generate_series(1, 400000) i;
 CREATE INDEX rsv_k ON rsv (k);
 ANALYZE rsv;
}

teardown
{
 DROP TABLE rsv;
}

session s1
setup		{ SET enable_seqscan = off; SET enable_bitmapscan = off; SET enable_indexonlyscan = off; SET enable_sort = off; }
step s1b	{ BEGIN ISOLATION LEVEL REPEATABLE READ; SELECT count(*) FROM rsv WHERE k < 0; }
step s1z	{ SET max_parallel_workers_per_gather = 0; }
step s1w0	{ SELECT id, k FROM rsv WHERE k BETWEEN 1000 AND 760000 AND k % 49999 < 2 ORDER BY k; }
step s1e	{ SET max_parallel_workers_per_gather = 8; }
step s1w8	{ SELECT id, k FROM rsv WHERE k BETWEEN 1000 AND 760000 AND k % 49999 < 2 ORDER BY k /* w8 */; }
step s1c	{ COMMIT; }

session s2
step s2del	{ DELETE FROM rsv WHERE k BETWEEN 40000 AND 100000 OR k % 6 = 0; }
step s2ins	{ INSERT INTO rsv SELECT 1000000 + i, i * 2 + 1, 'q' || i FROM generate_series(400, 380000) i; }
step s2bi	{ BEGIN; INSERT INTO rsv SELECT 2000000 + i, i * 2 + 1, 'r' || i FROM generate_series(400, 380000) i; }
step s2a	{ ROLLBACK; }

session s3
step s3vac	{ VACUUM rsv; }

permutation s2del s1b s3vac s2ins s1z s1w0 s1e s1w8 s1c s1z s1w0 s1e s1w8
permutation s1b s2bi s1z s1w0 s2a s3vac s1e s1w8 s1c s1z s1w0 s1e s1w8
permutation s2del s1b s1z s1w0 s3vac s1e s1w8 s2ins s1z s1w0 s1e s1w8 s1c s1w8

# An order-free full btree walk (no index qual) served as the heap range source, as a sequential scan
# would be: an Index Scan (frv_k) and an Index Only Scan (frv_kid, its visibility map set by the setup's VACUUM), each
# under an aggregate, while other sessions change the table under the reader's snapshot: HOT and non-HOT updates (the
# second moves index entries), deletes, inserts, an uncommitted insert, and a VACUUM that removes dead rows and sets
# and clears visibility map bits. The reader's rows must equal a serial index walk's under the same snapshot, at W0 and
# at W8, before and after it commits.
#   (1) every change committed and vacuumed after the snapshot, then read inside and after it
#   (2) an uncommitted insert during the reads, rolled back, then a VACUUM and the reads again
#   (3) the changes before the snapshot (the snapshot sees them), a VACUUM inside it, more changes after
# Expected output is PostgreSQL 18.6's (the C oracle).
setup
{
 CREATE TABLE frv (id int NOT NULL, k int NOT NULL, pad text) WITH (fillfactor = 90, autovacuum_enabled = off);
 INSERT INTO frv SELECT i, i * 2, 'p' || i FROM generate_series(1, 150000) i;
 CREATE INDEX frv_k ON frv (k);
 CREATE INDEX frv_kid ON frv (k, id);
}
setup
{
 VACUUM ANALYZE frv;
}

teardown
{
 DROP TABLE frv;
}

session s1
setup		{ SET enable_seqscan = off; SET enable_bitmapscan = off; }
step s1b	{ BEGIN ISOLATION LEVEL REPEATABLE READ; SELECT count(*) FROM frv WHERE k < 0; }
step s1i0	{ SET max_parallel_workers_per_gather = 0; SET enable_indexonlyscan = off; SELECT count(*), sum(k::bigint), sum(id::bigint), min(pad), max(pad) FROM (SELECT k, id, pad FROM frv ORDER BY k) s; }
step s1i8	{ SET max_parallel_workers_per_gather = 8; SET enable_indexonlyscan = off; SELECT count(*), sum(k::bigint), sum(id::bigint), min(pad), max(pad) FROM (SELECT k, id, pad FROM frv ORDER BY k) s; }
step s1o0	{ SET max_parallel_workers_per_gather = 0; SET enable_indexonlyscan = on; SELECT count(*), sum(k::bigint), sum(id::bigint), min(k), max(id) FROM (SELECT k, id FROM frv ORDER BY k, id) s; }
step s1o8	{ SET max_parallel_workers_per_gather = 8; SET enable_indexonlyscan = on; SELECT count(*), sum(k::bigint), sum(id::bigint), min(k), max(id) FROM (SELECT k, id FROM frv ORDER BY k, id) s; }
step s1c	{ COMMIT; }

session s2
step s2hot	{ UPDATE frv SET pad = pad || 'h' WHERE id % 5 = 0; }
step s2mov	{ UPDATE frv SET k = k + 1 WHERE id % 7 = 0; }
step s2del	{ DELETE FROM frv WHERE id % 11 = 0 OR id BETWEEN 60000 AND 70000; }
step s2ins	{ INSERT INTO frv SELECT 1000000 + i, i * 2 + 1, 'q' || i FROM generate_series(1, 40000) i; }
step s2bi	{ BEGIN; INSERT INTO frv SELECT 2000000 + i, i * 2 + 1, 'r' || i FROM generate_series(1, 40000) i; }
step s2a	{ ROLLBACK; }

session s3
step s3vac	{ VACUUM frv; }

permutation s1b s2hot s2mov s2del s2ins s3vac s1i0 s1i8 s1o0 s1o8 s1c s1i0 s1i8 s1o0 s1o8
permutation s1b s2bi s1i8 s1o8 s2a s3vac s1i0 s1o0 s1c s1i8 s1o8
permutation s2hot s2del s1b s3vac s1i8 s1o8 s2mov s2ins s1i0 s1o0 s1c s1i8 s1o8

# An order-free Index Only Scan read by the pool by leaf pages (every participant seizes leaves of one shared parallel
# btree walk; an entry whose heap page the visibility map vouches for is read from the index tuple, the others from the
# heap under the snapshot), with an index qual and without one, each under an aggregate, while other sessions change
# the table under the reader's snapshot: HOT and non-HOT updates (the second moves index entries), deletes, inserts, an
# uncommitted insert, and a VACUUM that removes dead rows and sets and clears visibility map bits. The reader's rows must
# equal a serial index-only walk's under the same snapshot, at W0 and at W8, before and after it commits.
#   (1) every change committed and vacuumed after the snapshot, then read inside and after it
#   (2) an uncommitted insert during the reads, rolled back, then a VACUUM and the reads again
#   (3) the changes before the snapshot (the snapshot sees them), a VACUUM inside it, more changes after
# Expected output is PostgreSQL 18.6's (the C oracle).
setup
{
 CREATE TABLE lvm (id int NOT NULL, k int NOT NULL, w text) WITH (fillfactor = 90, autovacuum_enabled = off);
 INSERT INTO lvm SELECT i, i * 2, repeat(md5(i::text), 8) FROM generate_series(1, 150000) i;
 CREATE INDEX lvm_kid ON lvm (k, id);
}
setup
{
 VACUUM ANALYZE lvm;
}

teardown
{
 DROP TABLE lvm;
}

session s1
setup		{ SET enable_seqscan = off; SET enable_bitmapscan = off; }
step s1b	{ BEGIN ISOLATION LEVEL REPEATABLE READ; SELECT count(*) FROM lvm WHERE k < 0; }
step s1q0	{ SET max_parallel_workers_per_gather = 0; SELECT count(*), sum(k::bigint), sum(id::bigint), min(k), max(id) FROM lvm WHERE k < 200000; }
step s1q8	{ SET max_parallel_workers_per_gather = 8; SELECT count(*), sum(k::bigint), sum(id::bigint), min(k), max(id) FROM lvm WHERE k < 200000; }
step s1f0	{ SET max_parallel_workers_per_gather = 0; SELECT count(*), sum(k::bigint), sum(id::bigint), min(k), max(id) FROM lvm; }
step s1f8	{ SET max_parallel_workers_per_gather = 8; SELECT count(*), sum(k::bigint), sum(id::bigint), min(k), max(id) FROM lvm; }
step s1c	{ COMMIT; }

session s2
step s2hot	{ UPDATE lvm SET w = w || 'h' WHERE id % 5 = 0; }
step s2mov	{ UPDATE lvm SET k = k + 1 WHERE id % 7 = 0; }
step s2del	{ DELETE FROM lvm WHERE id % 11 = 0 OR id BETWEEN 60000 AND 70000; }
step s2ins	{ INSERT INTO lvm SELECT 1000000 + i, i * 2 + 1, 'q' || i FROM generate_series(1, 40000) i; }
step s2bi	{ BEGIN; INSERT INTO lvm SELECT 2000000 + i, i * 2 + 1, 'r' || i FROM generate_series(1, 40000) i; }
step s2a	{ ROLLBACK; }

session s3
step s3vac	{ VACUUM lvm; }

permutation s1b s2hot s2mov s2del s2ins s3vac s1q0 s1q8 s1f0 s1f8 s1c s1q0 s1q8 s1f0 s1f8
permutation s1b s2bi s1q8 s1f8 s2a s3vac s1q0 s1f0 s1c s1q8 s1f8
permutation s2hot s2del s1b s3vac s1q8 s1f8 s2mov s2ins s1q0 s1f0 s1c s1q8 s1f8

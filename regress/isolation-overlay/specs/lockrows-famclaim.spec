# lockrows FAMCLAIM (gen lk-0116): the claim's IN form over a partitioned table (Limit -> LockRows -> Sort -> Append of each
# leaf's scan under a Subquery Scan, a HashAggregate over it, a Nested Loop into every leaf's primary key index scan,
# pruned at run time). The subquery's LockRows keeps its own row mark and rechecks under its own EvalPlanQual (in the IR
# over the arm the row's tableoid names): a row another transaction holds is skipped (SKIP LOCKED) or waited for (FOR
# UPDATE); after the wait the newest version is rechecked against the subquery's quals: one that no longer qualifies
# (its v made negative, deleted) is passed over and the Limit takes the next in sort order, one whose key changed but
# still qualifies is claimed at its new key, and one moved to another partition raises 40001. Rows 33 (lkc_a), 83 and 133
# (lkc_b) and 183 (lkc_d) qualify; s1 holds row 83. Steps that return rows claim one (LIMIT 1), so no hash order shows.
# D3379 FAMCLAIM HASH: s2h / s2hk claim 9 rows (33 .. 433), which the planner hashes; the targets' Append probes it (Hash
# Semi Join), the lock runs when the first target row comes, and the write's own recheck follows the claimed row's new
# version (s1n), passes over a row that no longer qualifies (s1d, s1x: the Limit takes 483), claims the old version after
# s1 rolls back, and raises on a row moved to another partition (s1m). They return no rows; s3sel reads the result back.

setup
{
 CREATE TABLE lkc (d date NOT NULL, id int NOT NULL, v int, PRIMARY KEY (d, id)) PARTITION BY RANGE (d);
 CREATE TABLE lkc_a PARTITION OF lkc FOR VALUES FROM ('2024-01-01') TO ('2024-03-01');
 CREATE TABLE lkc_b PARTITION OF lkc FOR VALUES FROM ('2024-03-01') TO ('2024-06-01');
 CREATE TABLE lkc_d PARTITION OF lkc DEFAULT;
 INSERT INTO lkc SELECT date '2024-01-01' + (i % 200), i, i FROM generate_series(1, 6000) i;
 ANALYZE lkc;
}

teardown
{
 DROP TABLE lkc;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1n	{ UPDATE lkc SET v = v + 1 WHERE id = 83; }
step s1d	{ UPDATE lkc SET v = -v WHERE id = 83; }
step s1x	{ DELETE FROM lkc WHERE id = 83; }
step s1k	{ UPDATE lkc SET id = id + 1000 WHERE id = 83; }
step s1m	{ UPDATE lkc SET d = d + 100 WHERE id = 83; }
step s1c	{ COMMIT; }
step s1a	{ ROLLBACK; }

session s2
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s2e	{ EXPLAIN (COSTS OFF) UPDATE lkc SET v = -v WHERE (d, id) IN (SELECT d, id FROM lkc WHERE v > 0 AND id % 50 = 33 AND id > 50 ORDER BY id LIMIT 1 FOR UPDATE) RETURNING id, v; }
step s2r	{ UPDATE lkc SET v = -v WHERE (d, id) IN (SELECT d, id FROM lkc WHERE v > 0 AND id % 50 = 33 AND id > 50 ORDER BY id LIMIT 1 FOR UPDATE) RETURNING id, v, tableoid::regclass AS leaf; }
step s2u	{ UPDATE lkc SET v = -v WHERE (d, id) IN (SELECT d, id FROM lkc WHERE v > 0 AND id % 50 = 33 ORDER BY id LIMIT 3 FOR UPDATE); }
step s2k	{ UPDATE lkc SET v = -v WHERE (d, id) IN (SELECT d, id FROM lkc WHERE v > 0 AND id % 50 = 33 AND id > 50 ORDER BY id LIMIT 1 FOR UPDATE SKIP LOCKED) RETURNING id, v; }
step s2x	{ DELETE FROM lkc WHERE (d, id) IN (SELECT d, id FROM lkc WHERE v > 0 AND id % 50 = 33 AND id > 50 ORDER BY id LIMIT 1 FOR UPDATE) RETURNING id, d; }
step s2he	{ EXPLAIN (COSTS OFF) UPDATE lkc SET v = -v WHERE (d, id) IN (SELECT d, id FROM lkc WHERE v > 0 AND id % 50 = 33 ORDER BY id LIMIT 9 FOR UPDATE); }
step s2h	{ UPDATE lkc SET v = -v WHERE (d, id) IN (SELECT d, id FROM lkc WHERE v > 0 AND id % 50 = 33 ORDER BY id LIMIT 9 FOR UPDATE); }
step s2hk	{ UPDATE lkc SET v = -v WHERE (d, id) IN (SELECT d, id FROM lkc WHERE v > 0 AND id % 50 = 33 ORDER BY id LIMIT 9 FOR UPDATE SKIP LOCKED); }
step s2c	{ COMMIT; }

session s3
step s3sel	{ SELECT tableoid::regclass AS leaf, id, v FROM lkc WHERE id % 50 = 33 AND id < 1200 OR v <= 0 ORDER BY id; }

permutation s2e s2c
permutation s1n s2r s1c s2c s3sel
permutation s1d s2r s1c s2c s3sel
permutation s1x s2r s1c s2c s3sel
permutation s1k s2r s1c s2c s3sel
permutation s1m s2r s1c s2c s3sel
permutation s1n s2r s1a s2c s3sel
permutation s1n s2k s1c s2c s3sel
permutation s1d s2x s1c s2c s3sel
permutation s1n s2u s1c s2c s3sel
permutation s1d s2u s1c s2c s3sel
permutation s2he s1c s2c
permutation s1n s2h s1c s2c s3sel
permutation s1d s2h s1c s2c s3sel
permutation s1x s2h s1c s2c s3sel
permutation s1n s2h s1a s2c s3sel
permutation s1m s2h s1c s2c s3sel
permutation s1n s2hk s1c s2c s3sel

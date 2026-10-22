# lockrows D2604: a row lock over a Sort of a partitioned / inheritance family's Append (ORDER BY .. FOR UPDATE over the
# tree, a cursor's too). ExecLockRows locks each row in the leaf its tableoid names; the other children's marks are
# inactive (curCtid invalid), so WHERE CURRENT OF finds the current row in its own leaf alone, and EvalPlanQual reruns the
# Sort over that leaf's test tuple (the other children give none). Forms: a concurrent update of a child's row (its newest
# version returned in its sorted place), one moving it out of the quals (dropped), the partitioned tree, the parent's own
# rows, cursors whose current row moves from one leaf to another before WHERE CURRENT OF, and with enable_sort off an
# ordered Append and a Merge Append of the leaves' index scans for the statement and a cursor. Expected output is the C oracle's (PostgreSQL 18,
# pgdg).
setup
{
  CREATE TABLE fi (k int, v text);
  CREATE TABLE fi_c (extra int DEFAULT 7) INHERITS (fi);
  CREATE TABLE fi_d (w int) INHERITS (fi);
  INSERT INTO fi VALUES (1, 'a'), (11, 'a');
  INSERT INTO fi_c (k, v) VALUES (2, 'c'), (12, 'c');
  INSERT INTO fi_d (k, v) VALUES (3, 'd'), (13, 'd');
  CREATE TABLE fp (k int, v text) PARTITION BY RANGE (k);
  CREATE TABLE fp1 PARTITION OF fp FOR VALUES FROM (0) TO (10);
  CREATE TABLE fp2 PARTITION OF fp FOR VALUES FROM (10) TO (20);
  INSERT INTO fp SELECT g, 'p' FROM generate_series(1, 15) g;
  CREATE INDEX fp_k ON fp (k);
  CREATE INDEX fp_vk ON fp (v, k);
  ANALYZE fi, fi_c, fi_d, fp;
}

teardown
{
  DROP TABLE fi CASCADE;
  DROP TABLE fp;
}

session s1
step s1b	{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1u	{ UPDATE fi SET v = v || '1' WHERE k = 12; }
step s1m	{ UPDATE fi SET k = 100 WHERE k = 12; }
step s1p	{ UPDATE fp SET v = 'q' WHERE k = 12; }
step s1c	{ COMMIT; }

session s2
step s2b	{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s2x	{ EXPLAIN (COSTS OFF) SELECT tableoid::regclass, k, v FROM fi WHERE k IN (2, 12, 13) ORDER BY k FOR UPDATE; }
step s2l	{ SELECT tableoid::regclass, k, v FROM fi WHERE k IN (2, 12, 13) ORDER BY k FOR UPDATE; }
step s2p	{ SELECT tableoid::regclass, k, v FROM fp WHERE k IN (2, 12, 13) ORDER BY v, k DESC FOR UPDATE; }
step s2d	{ DECLARE c CURSOR FOR SELECT k FROM fi WHERE k IN (1, 2, 3, 13) ORDER BY k FOR UPDATE; }
step s2f1	{ FETCH 1 FROM c; }
step s2w	{ UPDATE fi SET v = 'cur' WHERE CURRENT OF c; }
step s2r	{ SELECT tableoid::regclass, k, v FROM fi WHERE v = 'cur' ORDER BY k; }
step s2c	{ COMMIT; }

session s3
setup		{ SET enable_sort = off; SET enable_bitmapscan = off; SET enable_seqscan = off; }
step s3b	{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s3x	{ EXPLAIN (COSTS OFF) SELECT tableoid::regclass, k, v FROM fp WHERE k IN (2, 12, 13) ORDER BY k FOR UPDATE; }
step s3l	{ SELECT tableoid::regclass, k, v FROM fp WHERE k IN (2, 12, 13) ORDER BY k FOR UPDATE; }
step s3y	{ EXPLAIN (COSTS OFF) SELECT tableoid::regclass, k, v FROM fp WHERE k IN (2, 12, 13) ORDER BY v, k FOR UPDATE; }
step s3m	{ SELECT tableoid::regclass, k, v FROM fp WHERE k IN (2, 12, 13) ORDER BY v, k FOR UPDATE; }
step s3d	{ DECLARE m CURSOR FOR SELECT k FROM fp WHERE k IN (2, 9, 12, 13) ORDER BY v, k FOR UPDATE; }
step s3f	{ FETCH 1 FROM m; }
step s3w	{ UPDATE fp SET v = 'cur' WHERE CURRENT OF m; }
step s3r	{ SELECT tableoid::regclass, k, v FROM fp WHERE v = 'cur' ORDER BY k; }
step s3c	{ COMMIT; }

permutation s2b s2x s1b s1u s2l s1c s2c
permutation s2b s1b s1m s2l s1c s2c
permutation s2b s1b s1p s2p s1c s2c
permutation s2b s2d s2f1 s2w s2f1 s2f1 s2w s2r s2c
permutation s2b s2d s2f1 s2f1 s2f1 s2f1 s2w s2r s2c
permutation s3b s3x s1b s1p s3l s1c s3c
permutation s3b s3y s1b s1p s3m s1c s3c
permutation s3b s3d s3f s3f s3w s3f s3f s3w s3r s3c

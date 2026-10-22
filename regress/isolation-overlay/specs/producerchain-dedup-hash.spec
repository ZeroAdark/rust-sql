# producerchain B3 (heldout orm-0110): UPDATE / DELETE t .. WHERE t.id IN (SELECT s.k FROM s JOIN r ON ..), the semi
# join's inner a Hash Join of s's scan and r's filtered scan (the build), unique-ified by Unique over a Sort on the key
# (enable_hashagg off; a HashAggregate there keeps D2194's name; s4u: over a nested loop of s and r's index scan instead,
# which the recheck scans again), as the outer of a nested loop whose inner is t's index scan. Two s rows per key: the
# Unique keeps the first. s and r are marked ROW_MARK_REFERENCE: EvalPlanQual refetches each at its junk ctid (the
# version the statement joined, s1's change to it unseen), the Sort over their join yields the one row again, and t's
# index scan rechecks its key over t's newest version. A version that still matches is written over its new values, a
# deleted one or one whose key moved is skipped.

setup
{
 CREATE TABLE ht (id int PRIMARY KEY, st text, v int);
 INSERT INTO ht SELECT i, 'new', 0 FROM generate_series(1, 2000) i;
 CREATE TABLE hs (id int PRIMARY KEY, bid int, cid int, flag text);
 INSERT INTO hs SELECT i, (i - 1) % 2000 + 1, i, CASE WHEN ((i - 1) % 2000 + 1) % 100 = 0 THEN 'x' ELSE 'y' END FROM generate_series(1, 4000) i;
 CREATE TABLE hc (id int PRIMARY KEY, acct int);
 INSERT INTO hc SELECT i, CASE WHEN i % 100 = 0 THEN 0 ELSE i % 7 + 1 END FROM generate_series(1, 4000) i;
 ANALYZE ht, hs, hc;
}

teardown
{
 DROP TABLE ht, hs, hc;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1x	{ UPDATE ht SET v = v + 10 WHERE id IN (100, 200, 300); }
step s1f	{ UPDATE hs SET flag = 'y' WHERE bid = 200; }
step s1g	{ UPDATE hc SET acct = 5 WHERE id IN (300, 2300); }
step s1d	{ DELETE FROM ht WHERE id = 400; }
step s1k	{ UPDATE ht SET id = id + 10000 WHERE id = 600; }
step s1c	{ COMMIT; }

session s2
setup		{ SET enable_mergejoin = off; SET enable_hashagg = off; SET max_parallel_workers_per_gather = 0; BEGIN ISOLATION LEVEL READ COMMITTED; }
step s2u	{ UPDATE ht SET st = 'paid', v = ht.v + 1 WHERE ht.id IN (SELECT hs.bid FROM hs JOIN hc ON hc.id = hs.cid WHERE hs.flag = 'x' AND hc.acct = 0); }
step s2d	{ DELETE FROM ht WHERE ht.id IN (SELECT hs.bid FROM hs JOIN hc ON hc.id = hs.cid WHERE hs.flag = 'x' AND hc.acct = 0); }
step s2c	{ COMMIT; }

session s4
setup		{ SET enable_mergejoin = off; SET enable_hashjoin = off; SET enable_hashagg = off; SET enable_material = off; SET max_parallel_workers_per_gather = 0; BEGIN ISOLATION LEVEL READ COMMITTED; }
step s4u	{ UPDATE ht SET st = 'nl', v = ht.v + 4 WHERE ht.id IN (SELECT hs.bid FROM hs JOIN hc ON hc.id = hs.cid WHERE hs.flag = 'x' AND hc.acct = 0); }
step s4c	{ COMMIT; }

session s3
setup		{ SET enable_mergejoin = off; SET max_parallel_workers_per_gather = 0; }
step s3sel	{ SELECT id, st, v FROM ht WHERE id % 100 = 0 OR id > 10000 ORDER BY id; }
step s3pu	{ SET enable_hashagg = off; EXPLAIN (COSTS OFF) UPDATE ht SET st = 'paid', v = ht.v + 1 WHERE ht.id IN (SELECT hs.bid FROM hs JOIN hc ON hc.id = hs.cid WHERE hs.flag = 'x' AND hc.acct = 0); }
step s3pn	{ SET enable_hashagg = off; SET enable_hashjoin = off; SET enable_material = off; EXPLAIN (COSTS OFF) UPDATE ht SET st = 'nl', v = ht.v + 4 WHERE ht.id IN (SELECT hs.bid FROM hs JOIN hc ON hc.id = hs.cid WHERE hs.flag = 'x' AND hc.acct = 0); }

permutation s3pu s3pn s1c s2c s4c
permutation s1x s1f s1g s1d s1k s2u s1c s2c s3sel s4c
permutation s2u s1x s2c s1c s3sel s4c
permutation s1x s1d s1k s2d s1c s2c s3sel s4c
permutation s1x s1f s1g s1d s1k s4u s1c s4c s3sel s2c

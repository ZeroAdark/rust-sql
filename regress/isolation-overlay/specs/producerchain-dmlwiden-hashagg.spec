# producerchain (main's GO 10-04: nest_write.rs `deferred` lifted for UPDATE / DELETE): the B3 form of
# producerchain-dedup-hash with enable_hashagg on, so a HashAggregate (a grouping) de-duplicates the semi join's inner, a
# Hash Join of s's scan and r's filtered scan (the build), as the outer of a nested loop whose inner is t's index scan.
# D2194's guard (a grouping beside a hash join) refused it by name; D2194 / D2147 / D2201 are closed and D2325 FIXED.
# s and r are marked ROW_MARK_REFERENCE: EvalPlanQual refetches each at its junk ctid (the version the statement joined,
# s1's change to it unseen), the HashAggregate over their join yields the key again, and t's index scan rechecks its key
# over t's newest version. A version that still matches is written over its new values, a deleted one or one whose key
# moved is skipped.

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
setup		{ SET enable_mergejoin = off; SET enable_hashagg = on; SET max_parallel_workers_per_gather = 0; BEGIN ISOLATION LEVEL READ COMMITTED; }
step s2u	{ UPDATE ht SET st = 'paid', v = ht.v + 1 WHERE ht.id IN (SELECT hs.bid FROM hs JOIN hc ON hc.id = hs.cid WHERE hs.flag = 'x' AND hc.acct = 0); }
step s2d	{ DELETE FROM ht WHERE ht.id IN (SELECT hs.bid FROM hs JOIN hc ON hc.id = hs.cid WHERE hs.flag = 'x' AND hc.acct = 0); }
step s2c	{ COMMIT; }

session s3
setup		{ SET enable_mergejoin = off; SET max_parallel_workers_per_gather = 0; }
step s3sel	{ SELECT id, st, v FROM ht WHERE id % 100 = 0 OR id > 10000 ORDER BY id; }
step s3pu	{ SET enable_hashagg = on; EXPLAIN (COSTS OFF) UPDATE ht SET st = 'paid', v = ht.v + 1 WHERE ht.id IN (SELECT hs.bid FROM hs JOIN hc ON hc.id = hs.cid WHERE hs.flag = 'x' AND hc.acct = 0); }

permutation s3pu s1c s2c
permutation s1x s1f s1g s1d s1k s2u s1c s2c s3sel
permutation s2u s1x s2c s1c s3sel
permutation s1x s1d s1k s2d s1c s2c s3sel

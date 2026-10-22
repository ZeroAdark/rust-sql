# producerchain B2 (heldout orm-0108 / tw-core-107): UPDATE / DELETE t .. WHERE t.id IN (SELECT s.k FROM s JOIN r ON ..):
# PostgreSQL 18.6 unique-ifies the semi join's inner, a nested loop of s's scan and r's index scan, under a HashAggregate
# on the key (each relation's ctid junk an unkeyed cell), as the outer of a nested loop whose inner is t's index scan.
# s and r are marked ROW_MARK_REFERENCE: EvalPlanQual refetches each at its junk ctid (the version the statement joined,
# s1's change to it unseen), the HashAggregate over their join yields the one group again, and t's index scan rechecks
# its key over t's newest version. A version that still matches is written over its new values, a deleted one or one
# whose key moved is skipped. s2r: the subquery joins t to itself (t's other scan is s, by another alias).

setup
{
 CREATE TABLE bt (id int PRIMARY KEY, st text, v int);
 INSERT INTO bt SELECT i, 'new', 0 FROM generate_series(1, 2000) i;
 CREATE TABLE bs (id int PRIMARY KEY, bid int, cid int, flag text);
 INSERT INTO bs SELECT i, i, i, CASE WHEN i % 100 = 0 THEN 'x' ELSE 'y' END FROM generate_series(1, 2000) i;
 CREATE INDEX bs_flag ON bs (flag);
 CREATE TABLE bc (id int PRIMARY KEY, region text);
 INSERT INTO bc SELECT i, CASE WHEN i % 2 = 0 THEN 'south' ELSE 'north' END FROM generate_series(0, 4999) i;
 ANALYZE bt, bs, bc;
}

teardown
{
 DROP TABLE bt, bs, bc;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1x	{ UPDATE bt SET v = v + 10 WHERE id IN (100, 200, 300); }
step s1f	{ UPDATE bs SET flag = 'y' WHERE id = 200; }
step s1g	{ UPDATE bc SET region = 'north' WHERE id = 300; }
step s1d	{ DELETE FROM bt WHERE id = 400; }
step s1k	{ UPDATE bt SET id = id + 10000 WHERE id = 600; }
step s1c	{ COMMIT; }

session s2
setup		{ SET enable_hashjoin = off; SET enable_mergejoin = off; SET max_parallel_workers_per_gather = 0; BEGIN ISOLATION LEVEL READ COMMITTED; }
step s2u	{ UPDATE bt SET st = 'paid', v = bt.v + 1 WHERE bt.id IN (SELECT bs.bid FROM bs JOIN bc ON bc.id = bs.cid WHERE bs.flag = 'x' AND bc.region = 'south'); }
step s2d	{ DELETE FROM bt WHERE bt.id IN (SELECT bs.bid FROM bs JOIN bc ON bc.id = bs.cid WHERE bs.flag = 'x' AND bc.region = 'south'); }
step s2r	{ UPDATE bt SET st = 'self', v = bt.v + 2 WHERE bt.id IN (SELECT b2.id FROM bt b2 JOIN bc ON bc.id = b2.id WHERE b2.id % 100 = 0 AND bc.region = 'south'); }
step s2c	{ COMMIT; }

session s3
setup		{ SET enable_hashjoin = off; SET enable_mergejoin = off; SET max_parallel_workers_per_gather = 0; }
step s3sel	{ SELECT id, st, v FROM bt WHERE id % 100 = 0 OR id > 10000 ORDER BY id; }
step s3pu	{ EXPLAIN (COSTS OFF) UPDATE bt SET st = 'paid', v = bt.v + 1 WHERE bt.id IN (SELECT bs.bid FROM bs JOIN bc ON bc.id = bs.cid WHERE bs.flag = 'x' AND bc.region = 'south'); }
step s3pr	{ EXPLAIN (COSTS OFF) UPDATE bt SET st = 'self', v = bt.v + 2 WHERE bt.id IN (SELECT b2.id FROM bt b2 JOIN bc ON bc.id = b2.id WHERE b2.id % 100 = 0 AND bc.region = 'south'); }

permutation s3pu s3pr s1c s2c
permutation s1x s1f s1g s1d s1k s2u s1c s2c s3sel
permutation s2u s1x s2c s1c s3sel
permutation s1x s1d s1k s2d s1c s2c s3sel
permutation s1x s1f s1k s2r s1c s2c s3sel

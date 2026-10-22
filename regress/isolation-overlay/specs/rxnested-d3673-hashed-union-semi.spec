# D3673: UPDATE / DELETE of a partitioned table under EXISTS / NOT EXISTS / IN, its leaves hashed under the join (a Hash
# Right Semi / Anti Join over the Append of the leaves' scans, the larger relation probing; nested loops and merge
# joins off), while another session changes the target row (READ COMMITTED). PostgreSQL 18.6's EvalPlanQual rescans
# the row's own leaf at its newest version (every other leaf empty) and the probing relation at the row its REFERENCE
# mark fetched: a semi join's matched row must still match it; an anti join's row has no such row (its scan is
# empty), so the newest version passes. A row moved to another leaf raises the moved-partition error.
#  (a) a matched row updated in place     (b) a matched row re-keyed off its match     (c) a matched row deleted
#  (d) a matched row moved to another leaf
#  (e) NOT EXISTS: an unmatched row updated in place, and re-keyed onto a matching key (deleted all the same)
#  (f) IN with RETURNING over a row updated in place
# The EXPLAIN steps pin the plans. Expected output is PostgreSQL 18.6's (the C oracle).
setup
{
 CREATE TABLE up (k int, hits int NOT NULL DEFAULT 0, note text) PARTITION BY RANGE (k);
 CREATE TABLE up1 PARTITION OF up FOR VALUES FROM (0) TO (10);
 CREATE TABLE up2 PARTITION OF up FOR VALUES FROM (10) TO (20);
 CREATE TABLE up3 PARTITION OF up FOR VALUES FROM (20) TO (1000);
 INSERT INTO up SELECT i, 0, 'n' || i FROM generate_series(1, 23) i WHERE i % 10 BETWEEN 1 AND 3;
 CREATE TABLE us (k int, w int);
 INSERT INTO us VALUES (2, 1), (12, 2), (22, 3);
 INSERT INTO us SELECT i, 0 FROM generate_series(200, 259) i;
 ANALYZE up, us;
}

teardown
{
 DROP TABLE up, us;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1v	{ UPDATE up SET hits = 7 WHERE k = 12; }
step s1key	{ UPDATE up SET k = 14 WHERE k = 12; }
step s1del	{ DELETE FROM up WHERE k = 12; }
step s1mv	{ UPDATE up SET k = 15 WHERE k = 2; }
step s1nv	{ UPDATE up SET hits = 8 WHERE k = 13; }
step s1nkey	{ UPDATE up SET k = 12 WHERE k = 13; }
step s1c	{ COMMIT; }

session s2
setup		{ SET enable_nestloop = off; SET enable_mergejoin = off; }
step s2uexp	{ EXPLAIN (COSTS OFF) UPDATE up t SET hits = t.hits + 100 WHERE EXISTS (SELECT 1 FROM us s WHERE s.k = t.k); }
step s2u	{ UPDATE up t SET hits = t.hits + 100 WHERE EXISTS (SELECT 1 FROM us s WHERE s.k = t.k); }
step s2dexp	{ EXPLAIN (COSTS OFF) DELETE FROM up t WHERE NOT EXISTS (SELECT 1 FROM us s WHERE s.k = t.k); }
step s2d	{ DELETE FROM up t WHERE NOT EXISTS (SELECT 1 FROM us s WHERE s.k = t.k); }
step s2in	{ UPDATE up t SET note = 'in' || t.hits WHERE t.k IN (SELECT s.k FROM us s WHERE s.w > 0) RETURNING t.k, t.hits, t.note; }
step s2sel	{ SELECT tableoid::regclass, k, hits, note FROM up ORDER BY k, note; }

permutation s2uexp s1v s2u s1c s2sel
permutation s1key s2u s1c s2sel
permutation s1del s2u s1c s2sel
permutation s1mv s2u s1c s2sel
permutation s2dexp s1nv s2d s1c s2sel
permutation s1nkey s2d s1c s2sel
permutation s1v s2in s1c s2sel

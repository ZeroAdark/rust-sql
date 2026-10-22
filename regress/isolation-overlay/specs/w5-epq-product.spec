# W5 V4R-512 (D655's and D1496's lock halves): a LockRows whose rows feed another plan node's product (an Agg over a FOR
# UPDATE subquery, a CTE's tuplestore, a temporal FK's RI check: an Agg over a FOR KEY SHARE subquery). PostgreSQL's
# EvalPlanQual is the LockRows subtree's (EvalPlanQualInit over its outerPlan), and the parent gets only the EPQ row.
# s1 changes a row between s2's read and s2's lock, in each of these ways:
# - a new v in the group: the Agg's count and sum see the new version, never the old one beside it;
# - the row moved out of the group: the recheck fails, the row is not counted;
# - the row deleted: skipped;
# - a key update of the referenced range: the RI check's recheck sees the shrunk range.
# The rechecked row also feeds the ops above the lock (a LIMIT and the outer qual over it: only the new v passes) and a
# hash join's build (s5, hash join forced): the join's count and sum see the new version.
# A CTE's LockRows locks only the rows its reader pulls: s1's NOWAIT probes after its commit find row 9 free and row 6
# held. Under REPEATABLE READ (s3), TM_Updated and TM_Deleted raise 40001. The EXPLAIN steps pin the plan shapes.
# Outer-first hash joins (HJ_BUILD_HASHTABLE fetches the outer's first row before it builds): over an empty outer the
# locking build side never runs, so s5 neither waits for s1's lock on row 3 nor holds row 6 (s1's NOWAIT gets it); a
# locking CTE outer over an empty build locks its first row only (s1's NOWAIT gets row 6, not row 3).
# Expected output is the C oracle's (PostgreSQL 18, pgdg).

setup
{
 CREATE TABLE ep (id int PRIMARY KEY, grp int, v int);
 INSERT INTO ep SELECT i, i % 3, i FROM generate_series(1, 30) i;
 CREATE TABLE eprng (id int4range, valid_at daterange, PRIMARY KEY (id, valid_at WITHOUT OVERLAPS));
 INSERT INTO eprng VALUES ('[1,2)', '[2020-01-01,2021-01-01)'), ('[1,2)', '[2021-01-01,2022-01-01)');
 CREATE TABLE epref (id int4range, valid_at daterange, parent_id int4range, PRIMARY KEY (id, valid_at WITHOUT OVERLAPS), FOREIGN KEY (parent_id, PERIOD valid_at) REFERENCES eprng (id, PERIOD valid_at));
 CREATE TABLE eq (id int PRIMARY KEY, grp int, v int);
 INSERT INTO eq SELECT i, i % 3, i FROM generate_series(1, 30) i;
 ANALYZE eq;
 CREATE TABLE epe (id int);
}

teardown
{
 DROP TABLE epe;
 DROP TABLE eq;
 DROP TABLE epref;
 DROP TABLE eprng;
 DROP TABLE ep;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1v	{ UPDATE ep SET v = v + 1000 WHERE id = 3; }
step s1g	{ UPDATE ep SET grp = 1 WHERE id = 6; }
step s1d	{ DELETE FROM ep WHERE id = 9; }
step s1r	{ UPDATE eprng SET valid_at = '[2021-01-01,2021-06-01)' WHERE id = '[1,2)' AND valid_at = '[2021-01-01,2022-01-01)'; }
step s1c	{ COMMIT; }
step s1nw9	{ SELECT id FROM ep WHERE id = 9 FOR UPDATE NOWAIT; }
step s1nw6	{ SELECT id FROM ep WHERE id = 6 FOR UPDATE NOWAIT; }
step s1q3	{ SELECT id FROM eq WHERE id = 3 FOR UPDATE; }
step s1qn3	{ SELECT id FROM eq WHERE id = 3 FOR UPDATE NOWAIT; }
step s1qn6	{ SELECT id FROM eq WHERE id = 6 FOR UPDATE NOWAIT; }

session s2
step s2b	{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s2xa	{ EXPLAIN (COSTS OFF) SELECT count(*), sum(v) FROM (SELECT v FROM ep WHERE grp = 0 FOR UPDATE) s; }
step s2agg	{ SELECT count(*), sum(v) FROM (SELECT v FROM ep WHERE grp = 0 FOR UPDATE) s; }
step s2xc	{ EXPLAIN (COSTS OFF) WITH c AS MATERIALIZED (SELECT id, v FROM ep WHERE grp = 0 FOR UPDATE) SELECT id, v FROM c LIMIT 2; }
step s2cte	{ WITH c AS MATERIALIZED (SELECT id, v FROM ep WHERE grp = 0 FOR UPDATE) SELECT id, v FROM c LIMIT 2; }
step s2xl	{ EXPLAIN (COSTS OFF) SELECT count(*), sum(v) FROM (SELECT v FROM ep WHERE grp = 0 FOR UPDATE LIMIT 20) s WHERE v > 100; }
step s2lim	{ SELECT count(*), sum(v) FROM (SELECT v FROM ep WHERE grp = 0 FOR UPDATE LIMIT 20) s WHERE v > 100; }
step s2fk	{ INSERT INTO epref VALUES ('[1,2)', '[2020-06-01,2021-09-01)', '[1,2)'); }
step s2fkok	{ INSERT INTO epref VALUES ('[2,3)', '[2020-06-01,2021-03-01)', '[1,2)'); }
step s2c	{ COMMIT; }

session s3
step s3b	{ BEGIN ISOLATION LEVEL REPEATABLE READ; }
step s3snap	{ SELECT count(*) FROM ep; }
step s3agg	{ SELECT count(*), sum(v) FROM (SELECT v FROM ep WHERE grp = 0 FOR UPDATE) s; }
step s3c	{ COMMIT; }

session s4
step s4sel	{ SELECT id, grp, v FROM ep WHERE grp = 0 OR id IN (6, 9) ORDER BY id; }

session s5
setup		{ SET enable_nestloop = off; SET enable_mergejoin = off; }
step s5b	{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s5x	{ EXPLAIN (COSTS OFF) SELECT count(*), sum(s.v) FROM ep a JOIN (SELECT id, v FROM ep WHERE grp = 0 FOR UPDATE) s ON s.id = a.id; }
step s5hj	{ SELECT count(*), sum(s.v) FROM ep a JOIN (SELECT id, v FROM ep WHERE grp = 0 FOR UPDATE) s ON s.id = a.id; }
step s5xe	{ EXPLAIN (COSTS OFF) SELECT count(*), sum(s.v) FROM epe a JOIN (SELECT id, v FROM eq WHERE grp = 0 FOR UPDATE) s ON s.id = a.id; }
step s5e	{ SELECT count(*), sum(s.v) FROM epe a JOIN (SELECT id, v FROM eq WHERE grp = 0 FOR UPDATE) s ON s.id = a.id; }
step s5xo	{ EXPLAIN (COSTS OFF) WITH c AS MATERIALIZED (SELECT id, v FROM eq WHERE grp = 0 FOR UPDATE) SELECT count(*) FROM c JOIN eq b ON b.id = c.id + 1 WHERE b.id < 0; }
step s5o	{ WITH c AS MATERIALIZED (SELECT id, v FROM eq WHERE grp = 0 FOR UPDATE) SELECT count(*) FROM c JOIN eq b ON b.id = c.id + 1 WHERE b.id < 0; }
step s5c	{ COMMIT; }

# the Agg sees the rechecked row only: count 10, sum 165 + 1000
permutation s2b s2xa s1v s2agg s1c s2c s4sel
# the row left the group: the recheck fails (count 9)
permutation s2b s1g s2agg s1c s2c s4sel
# the row deleted: skipped (count 9)
permutation s2b s1d s2agg s1c s2c s4sel
# a CTE read by LIMIT 2 locks rows 3 and 6 only (row 3 rechecked at its new v): 9 stays free, 6 is held
permutation s2b s2xc s1v s2cte s1c s1nw9 s1nw6 s2c
# the RI check rechecks the shrunk range: the FK fails; a period inside the kept part passes
permutation s2b s1r s2fk s1c s2c
permutation s2b s1r s2fkok s1c s2c
# the ops above the lock get the rechecked row: row 3's new v passes the outer qual (count 1, sum 1003)
permutation s2b s2xl s1v s2lim s1c s2c
# a hash join's build gets the rechecked row: count 10, sum 1165
permutation s5b s5x s1v s5hj s1c s5c
# an empty outer never runs the locking build: s5 does not wait for row 3, and row 6 stays free for s1's NOWAIT
permutation s5b s5xe s1q3 s5e s1qn6 s1c s5c
# a locking CTE outer over an empty build locks its first row only: row 6 is free for s1's NOWAIT, row 3 is not
permutation s5b s5xo s5o s1qn6 s1qn3 s1c s5c
# REPEATABLE READ: TM_Updated / TM_Deleted -> could not serialize access (40001)
permutation s3b s3snap s1v s3agg s1c s3c
permutation s3b s3snap s1d s3agg s1c s3c

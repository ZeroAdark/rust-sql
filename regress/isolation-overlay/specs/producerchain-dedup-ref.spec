# producerchain B: an UPDATE / DELETE whose rows come from a nested loop whose outer is the HashAggregate that
# de-duplicates a semi join's inner (`WHERE id IN (SELECT qs.id FROM qs ...)`, qs REFERENCE-marked, its ctid a non-key
# column of the group) and whose inner is the target's scan: an index scan keyed by the group's key, or a TID scan
# (REFRESH MATERIALIZED VIEW CONCURRENTLY's `DELETE FROM mv WHERE ctid = ANY (SELECT diff.tid FROM diff ...)`). s1
# changes a target row the statement joins. EvalPlanQual then reruns the plan: qs's row is refetched at its junk ctid
# (the version the statement joined), so the HashAggregate over it yields the group the row joined, and the target's
# scan returns the newest version, its key checked again (IndexRecheck; TidRecheck tests the newest version's ctid
# against the list). A version that still passes is written, SET over its new values. One whose key moved off the
# joined value, that was deleted, or (TID form) whose newest version sits at another ctid is skipped. s1's change to qs
# is not seen.

setup
{
 CREATE TABLE qt (id int PRIMARY KEY, k int NOT NULL, x int NOT NULL);
 INSERT INTO qt SELECT i, i % 10, 0 FROM generate_series(1, 2000) i;
 CREATE TABLE qs (id int, w int NOT NULL);
 INSERT INTO qs SELECT i % 7, i FROM generate_series(1, 30) i;
 CREATE TABLE qm AS SELECT * FROM qt WHERE id <= 50;
 CREATE TABLE qd AS SELECT qm.ctid AS tid FROM qm WHERE id IN (2, 3, 4, 5);
 INSERT INTO qd SELECT qm.ctid FROM qm WHERE id = 4;
 ANALYZE qt, qs, qd;
}

teardown
{
 DROP TABLE qt;
 DROP TABLE qs;
 DROP TABLE qm;
 DROP TABLE qd;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1x	{ UPDATE qt SET x = x + 1000 WHERE id = 3; }
step s1i	{ UPDATE qt SET id = 3000 WHERE id = 4; }
step s1d	{ DELETE FROM qt WHERE id = 5; }
step s1s	{ UPDATE qs SET w = w - 100 WHERE id = 3; }
step s1m	{ UPDATE qm SET x = 1 WHERE id = 3; }
step s1n	{ DELETE FROM qm WHERE id = 5; }
step s1c	{ COMMIT; }

session s2
setup		{ SET enable_hashjoin = off; SET enable_mergejoin = off; SET enable_material = off; SET enable_memoize = off; SET max_parallel_workers_per_gather = 0; BEGIN ISOLATION LEVEL READ COMMITTED; }
step s2u	{ UPDATE qt SET x = x + 1 WHERE id IN (SELECT qs.id FROM qs WHERE qs.w > 2); }
step s2d	{ DELETE FROM qt WHERE id IN (SELECT qs.id FROM qs WHERE qs.w < 30); }
step s2t	{ DELETE FROM qm WHERE ctid OPERATOR(pg_catalog.=) ANY (SELECT qd.tid FROM qd WHERE qd.tid IS NOT NULL); }
step s2c	{ COMMIT; }

session s3
setup		{ SET enable_hashjoin = off; SET enable_mergejoin = off; SET enable_material = off; SET enable_memoize = off; SET max_parallel_workers_per_gather = 0; }
step s3sel	{ SELECT count(*), sum(id), sum(x) FROM qt; }
step s3row	{ SELECT id, k, x FROM qt WHERE id IN (1, 2, 3, 4, 5, 6, 7, 3000) ORDER BY id; }
step s3m	{ SELECT id, x FROM qm WHERE id <= 7 ORDER BY id; }
step s3pu	{ EXPLAIN (COSTS OFF) UPDATE qt SET x = x + 1 WHERE id IN (SELECT qs.id FROM qs WHERE qs.w > 2); }
step s3pd	{ EXPLAIN (COSTS OFF) DELETE FROM qt WHERE id IN (SELECT qs.id FROM qs WHERE qs.w < 30); }
step s3pt	{ EXPLAIN (COSTS OFF) DELETE FROM qm WHERE ctid OPERATOR(pg_catalog.=) ANY (SELECT qd.tid FROM qd WHERE qd.tid IS NOT NULL); }

permutation s3pu s3pd s3pt s1c s2c
permutation s1x s1i s1d s1s s2u s1c s2c s3sel s3row
permutation s1x s1d s2d s1c s2c s3sel s3row
permutation s1m s1n s2t s1c s2c s3m
permutation s2u s1x s2c s1c s3sel s3row
permutation s2t s1m s2c s1c s3m

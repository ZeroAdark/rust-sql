# producerchain G4: an UPDATE / DELETE whose target's scan is the inner of a nested loop after an INNER hash join of two
# other relations (`UPDATE ht ... FROM ha JOIN hb ON ...`: ha's scan probing a hash over hb's scan, then the target's
# bitmap heap scan keyed by the joined row), meets a target row s1 changed. EvalPlanQual reruns the plan with the target's
# newest version and the other relations' rows refetched at their junk ctids (the versions the statement joined): the
# build reruns over hb's one row and the probe joins ha's row to it again. A version whose key moved off the joined row
# is skipped, one that still joins is written (SET over its new values, hb's columns as the statement read them), a
# deleted one is skipped, and s1's changes to ha and hb (an hb row deleted or retagged included) are not seen.

setup
{
 CREATE TABLE ha (id int PRIMARY KEY, w int NOT NULL);
 INSERT INTO ha SELECT i, i % 3 FROM generate_series(1, 900) i;
 CREATE TABLE hb (id int PRIMARY KEY, a_id int NOT NULL, tag text NOT NULL);
 INSERT INTO hb VALUES (1, 1, 'b1'), (4, 4, 'b4'), (7, 7, 'b7'), (8, 2, 'b8');
 CREATE TABLE ht (id int PRIMARY KEY, a_id int NOT NULL, x int NOT NULL);
 INSERT INTO ht SELECT i, 1 + i % 900, 0 FROM generate_series(1, 9000) i;
 CREATE INDEX ht_a ON ht (a_id);
 ANALYZE ha, hb, ht;
}

teardown
{
 DROP TABLE ht;
 DROP TABLE hb;
 DROP TABLE ha;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1x	{ UPDATE ht SET x = x + 1000 WHERE id IN (900, 903, 906); }
step s1k	{ UPDATE ht SET a_id = 2 WHERE id = 1800; }
step s1j	{ UPDATE ht SET a_id = 4 WHERE id = 2700; }
step s1d	{ DELETE FROM ht WHERE id = 3600; }
step s1b	{ UPDATE hb SET tag = 'b4new' WHERE id = 4; }
step s1e	{ DELETE FROM hb WHERE id = 7; }
step s1a	{ UPDATE ha SET w = 0 WHERE id = 1; }
step s1c	{ COMMIT; }

session s2
setup		{ SET enable_mergejoin = off; SET enable_material = off; SET enable_memoize = off; SET max_parallel_workers_per_gather = 0; BEGIN ISOLATION LEVEL READ COMMITTED; }
step s2u	{ UPDATE ht SET x = x + 1 FROM ha JOIN hb ON hb.a_id = ha.id WHERE ht.a_id = ha.id AND ha.id IN (1, 4, 7) RETURNING ht.id, ht.a_id, ht.x, hb.tag, ha.w; }
step s2d	{ DELETE FROM ht USING ha JOIN hb ON hb.a_id = ha.id WHERE ht.a_id = ha.id AND ha.id IN (1, 4, 7) RETURNING ht.id, hb.tag; }
step s2c	{ COMMIT; }

session s3
setup		{ SET enable_mergejoin = off; SET enable_material = off; SET enable_memoize = off; SET max_parallel_workers_per_gather = 0; }
step s3sel	{ SELECT count(*), sum(a_id), sum(x) FROM ht; }
step s3row	{ SELECT id, a_id, x FROM ht WHERE id IN (900, 903, 906, 1800, 2700, 3600, 3) ORDER BY id; }
step s3pu	{ EXPLAIN (COSTS OFF) UPDATE ht SET x = x + 1 FROM ha JOIN hb ON hb.a_id = ha.id WHERE ht.a_id = ha.id AND ha.id IN (1, 4, 7); }
step s3pd	{ EXPLAIN (COSTS OFF) DELETE FROM ht USING ha JOIN hb ON hb.a_id = ha.id WHERE ht.a_id = ha.id AND ha.id IN (1, 4, 7); }

permutation s3pu s3pd s1c s2c
permutation s1x s1k s1j s1d s1b s1e s1a s2u s1c s2c s3sel s3row
permutation s1x s1k s1j s1d s1e s2d s1c s2c s3sel s3row
permutation s2u s1x s2c s1c s3sel s3row

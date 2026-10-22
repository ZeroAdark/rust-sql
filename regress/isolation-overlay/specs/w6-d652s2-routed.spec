# D652-S2 (modifytable, M2): an UPDATE / DELETE of a partitioned target whose SET list or WHERE reads a correlated
# SubPlan over an aggregate inner meets rows s1 changed in either partition (the second has its columns in another
# order). Each arm rechecks its own rows: the SubPlan is evaluated again over the newest version (its new grp: the
# other group's aggregate) under the statement's snapshot, a version that fails the WHERE stays, a deleted one is
# skipped.

setup
{
 CREATE TABLE pk (id int NOT NULL, grp int, val int, pad text) PARTITION BY RANGE (id);
 CREATE TABLE pk_1 PARTITION OF pk FOR VALUES FROM (1) TO (5);
 CREATE TABLE pk_2 (pad text, val int, grp int, id int NOT NULL);
 ALTER TABLE pk ATTACH PARTITION pk_2 FOR VALUES FROM (5) TO (9);
 INSERT INTO pk SELECT i, i % 2, i * 10, 'p' || i FROM generate_series(1, 8) i;
 CREATE TABLE ps (grp int, v int);
 INSERT INTO ps VALUES (0, 10), (0, 20), (1, 100), (1, 200);
 ANALYZE pk, ps;
}

teardown
{
 DROP TABLE pk;
 DROP TABLE ps;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1g	{ UPDATE pk SET grp = 1 WHERE id = 2; }
step s1h	{ UPDATE pk SET grp = 0 WHERE id = 7; }
step s1v	{ UPDATE pk SET val = 1000 WHERE id = 3; }
step s1p	{ UPDATE pk SET pad = 'q' WHERE id = 5; }
step s1x	{ DELETE FROM pk WHERE id = 1; }
step s1i	{ INSERT INTO ps VALUES (1, 900); }
step s1c	{ COMMIT; }

session s2
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s2u	{ UPDATE pk t SET val = (SELECT max(v) FROM ps WHERE ps.grp = t.grp) WHERE t.id <= 7; }
step s2d	{ DELETE FROM pk t WHERE t.val < (SELECT max(v) FROM ps WHERE ps.grp = t.grp) AND t.id <= 7; }
step s2r	{ UPDATE pk t SET val = -t.val WHERE t.val < (SELECT avg(v) FROM ps WHERE ps.grp = t.grp) RETURNING id, grp, val; }
step s2c	{ COMMIT; }

session s3
step s3sel	{ SELECT id, grp, val, pad FROM pk ORDER BY id; }

permutation s1g s1h s1i s2u s1c s2c s3sel
permutation s1v s1p s1x s2d s1c s2c s3sel
permutation s1g s1h s1v s2r s1c s2c s3sel

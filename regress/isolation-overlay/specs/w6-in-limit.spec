# IN-LIMIT (modifytable, M4): an UPDATE / DELETE ... WHERE id IN (SELECT .. ORDER BY .. LIMIT n) planned as a nested
# loop over the unique-ified subquery (a HashAggregate de-duplicating the SubqueryScan's rows, the target's index scan
# the inner) meets target rows s1 changed. EvalPlanQual hands the SubqueryScan the subquery row stored with the joined
# row (ROW_MARK_COPY: the subquery is never evaluated again, so s1's change to ils is not seen) and rescans the target
# at the changed row's newest version: one whose key still matches is written, one whose key moved or that was deleted
# is skipped. s4: a partitioned target probing a hash built over the unique-ified rows of a plain table (its REFERENCE
# mark: the de-duplicating aggregate carries its ctid); a recheck refetches that table's row at the version the
# statement joined, so s1's change to it (which the subquery's filter would reject) is not seen.

setup
{
 CREATE TABLE ilk (id int PRIMARY KEY, n int NOT NULL, val int);
 INSERT INTO ilk SELECT i, i, i FROM generate_series(1, 3000) i;
 CREATE TABLE ils (id int NOT NULL, n int NOT NULL, val int);
 INSERT INTO ils SELECT i, i, (i * 7919) % 1000 FROM generate_series(1, 500) i;
 CREATE TABLE ilp (id int PRIMARY KEY, k int, v numeric(12,2)) PARTITION BY HASH (id);
 CREATE TABLE ilp_0 PARTITION OF ilp FOR VALUES WITH (MODULUS 4, REMAINDER 0);
 CREATE TABLE ilp_1 PARTITION OF ilp FOR VALUES WITH (MODULUS 4, REMAINDER 1);
 CREATE TABLE ilp_2 PARTITION OF ilp FOR VALUES WITH (MODULUS 4, REMAINDER 2);
 CREATE TABLE ilp_3 PARTITION OF ilp FOR VALUES WITH (MODULUS 4, REMAINDER 3);
 INSERT INTO ilp SELECT i, i % 50, i / 7.0 FROM generate_series(1, 30000) i;
 CREATE TABLE ilr (d date, id int, v numeric(12,2));
 INSERT INTO ilr SELECT date '2023-12-20' + (i % 230), 30000 + i, i / 5.0 FROM generate_series(1, 1500) i;
 ANALYZE ilk, ils, ilp, ilr;
}

teardown
{
 DROP TABLE ilk;
 DROP TABLE ils;
 DROP TABLE ilp;
 DROP TABLE ilr;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1v	{ UPDATE ilk SET val = val + 1000 WHERE id = 321; }
step s1k	{ UPDATE ilk SET id = 30284 WHERE id = 284; }
step s1d	{ DELETE FROM ilk WHERE id = 247; }
step s1s	{ UPDATE ils SET val = 0 WHERE id = 210; }
step s1pv	{ UPDATE ilp SET v = v + 5 WHERE id = 1005; }
step s1pk	{ UPDATE ilp SET k = 77 WHERE id = 1010; }
step s1pd	{ DELETE FROM ilp WHERE id = 1009; }
step s1ps	{ UPDATE ilr SET v = 999 WHERE id = 30010; }
step s1c	{ COMMIT; }

session s2
setup		{ SET enable_hashjoin = off; SET enable_mergejoin = off; BEGIN ISOLATION LEVEL READ COMMITTED; }
step s2p	{ EXPLAIN (COSTS OFF) UPDATE ilk SET n = n + 100000 WHERE id IN (SELECT id FROM ils ORDER BY val DESC, n LIMIT 6); }
step s2u	{ UPDATE ilk SET n = n + 100000 WHERE id IN (SELECT id FROM ils ORDER BY val DESC, n LIMIT 6); }
step s2d	{ DELETE FROM ilk WHERE id IN (SELECT id FROM ils ORDER BY val DESC, n LIMIT 6); }
step s2c	{ COMMIT; }

session s3
step s3sel	{ SELECT id, n, val FROM ilk WHERE id IN (136, 173, 210, 247, 284, 321, 457, 494, 30284) ORDER BY id; }
step s3p	{ SELECT id, k, v FROM ilp WHERE id IN (1005, 1009, 1010, 1011) ORDER BY id; }

session s4
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s4p	{ EXPLAIN (COSTS OFF) UPDATE ilp SET v = v + 1000 WHERE id IN (SELECT id - 29000 FROM ilr WHERE v < 178); }
step s4u	{ UPDATE ilp SET v = v + 1000 WHERE id IN (SELECT id - 29000 FROM ilr WHERE v < 178); }
step s4c	{ COMMIT; }

permutation s2p s1c s2c s4c
permutation s1v s1k s1d s1s s2u s1c s2c s4c s3sel
permutation s1v s1k s1d s2d s1c s2c s4c s3sel
permutation s2u s1v s2c s1c s4c s3sel
permutation s4p s1c s4c s2c
permutation s1pv s1pk s1pd s1ps s4u s1c s4c s2c s3p

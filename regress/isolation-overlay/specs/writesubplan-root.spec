# writesubplan (D652-S1 over an inheritance root that is its own one result relation): wr_r's child was dropped, so
# relhassubclass stays set and PostgreSQL expands the root (rootRelation the parent, one result relation its child RTE of
# the same relation). READ COMMITTED: s2's UPDATE / DELETE waits on s1's row locks; after s1 commits, EvalPlanQual
# re-runs the plan below the write over the newest version: rows 2 and 3 carry s1's v, row 4 (deleted) is skipped, and
# the RETURNING list's SubPlans (OLD / NEW over generate_series) read the row as written over that version. s2q's WHERE
# fails over the newest version of rows 2 and 3 (their v is no longer a multiple of 10). With s2 first, s1 waits.

setup
{
 CREATE TABLE wr_r (id int PRIMARY KEY, v int NOT NULL);
 CREATE TABLE wr_rc () INHERITS (wr_r);
 DROP TABLE wr_rc;
 INSERT INTO wr_r SELECT i, i * 10 FROM generate_series(1, 6) i;
}

teardown
{
 DROP TABLE wr_r;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1u	{ UPDATE wr_r SET v = v + 1 WHERE id IN (2, 3); }
step s1d	{ DELETE FROM wr_r WHERE id = 4; }
step s1c	{ COMMIT; }

session s2
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s2u	{ UPDATE wr_r SET v = v + 100 WHERE id IN (2, 3, 4, 5) RETURNING id, v, (SELECT old.v = new.v) AS same, (SELECT max(old.v + x) FROM generate_series(1, 3) x) AS old_max, (SELECT max(new.v + x) FROM generate_series(1, 3) x) AS new_max; }
step s2d	{ DELETE FROM wr_r WHERE id IN (2, 3, 4, 5) RETURNING id, (SELECT max(old.v + x) FROM generate_series(1, 3) x) AS old_max; }
step s2q	{ UPDATE wr_r SET v = v + 100 WHERE id IN (2, 3, 4, 5) AND v % 10 = 0 AND (SELECT count(*) FROM generate_series(1, v) x) > 0 RETURNING id, v, (SELECT max(new.v + x) FROM generate_series(1, 3) x) AS new_max; }
step s2c	{ COMMIT; }

session s3
step s3sel	{ SELECT id, v FROM wr_r ORDER BY id; }

permutation s1u s1d s2u s1c s2c s3sel
permutation s1u s1d s2d s1c s2c s3sel
permutation s1u s1d s2q s1c s2c s3sel
permutation s2u s1u s2c s1c s3sel

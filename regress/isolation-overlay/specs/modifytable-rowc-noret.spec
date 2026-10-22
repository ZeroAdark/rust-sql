# rowc-noret (modifytable, D655 Row C): a data-modifying CTE's DELETE / UPDATE with no RETURNING rechecks in the IR. s2's
# CTE waits on the rows s1 changed; after s1 commits, the recheck keeps a row s1 moved out of the qual, writes one s1
# changed in another column (the newest version), and skips one s1 deleted; the statement's own SELECT reads its snapshot.
# With s1 rolled back, s2 writes the old versions.

setup
{
 CREATE TABLE rk (k int PRIMARY KEY, v int NOT NULL, grp int NOT NULL);
 INSERT INTO rk SELECT g, g, g % 5 FROM generate_series(1, 200) g;
 ANALYZE rk;
}

teardown
{
 DROP TABLE rk;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1mv	{ UPDATE rk SET grp = 9 WHERE k = 11; }
step s1v	{ UPDATE rk SET v = 1000 WHERE k = 21; }
step s1d	{ DELETE FROM rk WHERE k = 31; }
step s1c	{ COMMIT; }
step s1r	{ ROLLBACK; }

session s2
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s2d	{ WITH d AS (DELETE FROM rk WHERE grp = 1) SELECT count(*) FROM rk; }
step s2u	{ WITH u AS (UPDATE rk SET v = v + 1 WHERE grp = 1) SELECT count(*), sum(v) FROM rk; }
step s2c	{ COMMIT; }

session s3
step s3sel	{ SELECT k, v, grp FROM rk WHERE k IN (1, 11, 21, 31, 41) ORDER BY k; }
step s3cnt	{ SELECT count(*), sum(v) FROM rk; }

permutation s1mv s1v s1d s2d s1c s2c s3sel s3cnt
permutation s1mv s1v s1d s2u s1c s2c s3sel s3cnt
permutation s1mv s1v s2d s1r s2c s3sel s3cnt

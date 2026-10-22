# D3397 ROWC-SUB (modifytable, D3379): a data-modifying CTE's DELETE / UPDATE whose qual holds an eager correlated SubPlan
# rechecks in the IR, the SubPlan evaluated again over the newest version (PostgreSQL's EvalPlanQual re-runs it with the
# new row's arguments, under the statement's snapshot). s2's CTE waits on the rows s1 changed; after s1 commits, the
# recheck keeps a row s1 moved to a group whose cap its value no longer passes, writes one s1 changed in another column
# (the newest version), skips one s1 deleted, and reads the cap s1 changed in the SubPlan's own table as the statement's
# snapshot has it. With s1 rolled back, s2 writes the old versions.

setup
{
 CREATE TABLE sk (k int PRIMARY KEY, v int NOT NULL, grp int NOT NULL);
 INSERT INTO sk SELECT g, g, g % 5 FROM generate_series(1, 200) g;
 CREATE TABLE sl (grp int PRIMARY KEY, cap int NOT NULL);
 INSERT INTO sl VALUES (0, 10), (1, 20), (2, 30), (3, 40), (4, 50), (9, 500);
 ANALYZE sk, sl;
}

teardown
{
 DROP TABLE sk, sl;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1mv	{ UPDATE sk SET grp = 9 WHERE k = 51; }
step s1v	{ UPDATE sk SET v = 1000 WHERE k = 61; }
step s1d	{ DELETE FROM sk WHERE k = 71; }
step s1cap	{ UPDATE sl SET cap = 1000 WHERE grp = 1; }
step s1c	{ COMMIT; }
step s1r	{ ROLLBACK; }

session s2
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s2d	{ WITH d AS (DELETE FROM sk t WHERE t.grp = 1 AND t.v > (SELECT l.cap FROM sl l WHERE l.grp = t.grp) * 2) SELECT count(*) FROM sk; }
step s2u	{ WITH u AS (UPDATE sk t SET v = t.v + 1 WHERE t.grp = 1 AND t.v > (SELECT l.cap FROM sl l WHERE l.grp = t.grp) * 2) SELECT count(*), sum(v) FROM sk; }
step s2c	{ COMMIT; }

session s3
step s3sel	{ SELECT k, v, grp FROM sk WHERE k IN (36, 41, 51, 61, 71, 81) ORDER BY k; }
step s3cnt	{ SELECT count(*), sum(v) FROM sk; }

permutation s1mv s1v s1d s1cap s2d s1c s2c s3sel s3cnt
permutation s1mv s1v s1d s1cap s2u s1c s2c s3sel s3cnt
permutation s1mv s1v s2d s1r s2c s3sel s3cnt

# writesubplan (D1112, nest.subplan B4's MERGE-action and RETURNING parts): a SubPlan in a MERGE action and in an
# UPDATE's RETURNING list meet rows s1 changed or deleted. READ COMMITTED: s2's MERGE / UPDATE waits on s1's row lock;
# after s1 commits, EvalPlanQual re-runs the plan below the write over the newest version. The MERGE then reads its
# actions from the first over that version, so the WHEN qual's SubPlan and the SET's read the new grp (s1's 7) and the
# first action takes rows 2 and 3; row 4, which s1 deleted, is no longer matched. The UPDATE's RETURNING SubPlan reads
# the row as written over the newest version. With s2 first, s1 waits on s2's row locks instead.

setup
{
 CREATE TABLE wr_t (id int PRIMARY KEY, grp int NOT NULL, v int NOT NULL);
 INSERT INTO wr_t SELECT i, i % 3, i * 10 FROM generate_series(1, 10) i;
 CREATE TABLE wr_s (id int PRIMARY KEY, d int NOT NULL);
 INSERT INTO wr_s SELECT i, i FROM generate_series(1, 10) i;
 CREATE TABLE wr_d (grp int PRIMARY KEY, w int NOT NULL);
 INSERT INTO wr_d SELECT i, i * 100 FROM generate_series(0, 2999) i;
 ANALYZE wr_t, wr_s, wr_d;
}

teardown
{
 DROP TABLE wr_t;
 DROP TABLE wr_s;
 DROP TABLE wr_d;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1u	{ UPDATE wr_t SET grp = 7, v = v + 1 WHERE id IN (2, 3); }
step s1d	{ DELETE FROM wr_t WHERE id = 4; }
step s1c	{ COMMIT; }

session s2
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s2m	{ MERGE INTO wr_t t USING wr_s s ON t.id = s.id WHEN MATCHED AND s.id BETWEEN 2 AND 4 AND EXISTS (SELECT 1 FROM wr_d d WHERE d.grp = t.grp AND d.w > 500) THEN UPDATE SET v = (SELECT d.w FROM wr_d d WHERE d.grp = t.grp + s.d) WHEN MATCHED AND s.id BETWEEN 2 AND 4 THEN UPDATE SET v = -1; }
step s2r	{ UPDATE wr_t SET v = v + 1000 WHERE id IN (2, 3, 4) RETURNING id, grp, v, (SELECT d.w FROM wr_d d WHERE d.grp = wr_t.grp) AS w; }
step s2c	{ COMMIT; }

session s3
step s3sel	{ SELECT id, grp, v FROM wr_t WHERE id BETWEEN 1 AND 5 ORDER BY id; }

permutation s1u s1d s2m s1c s2c s3sel
permutation s1u s1d s2r s1c s2c s3sel
permutation s2m s1u s2c s1c s3sel
permutation s2r s1d s2c s1c s3sel

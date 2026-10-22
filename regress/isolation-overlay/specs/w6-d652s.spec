# D652-S1 (modifytable, M2): an UPDATE / DELETE whose SET list or WHERE reads a correlated SubPlan over an aggregate inner
# meets a row s1 changed. Its EvalPlanQual recheck evaluates the SubPlan again over the newest version (its new grp: the
# other group's aggregate), under the statement's snapshot (s1's inserted inner rows stay invisible), and drops a row whose
# newest version fails the WHERE.

setup
{
 CREATE TABLE dk (id int PRIMARY KEY, grp int, val int);
 INSERT INTO dk SELECT i, i % 2, i * 10 FROM generate_series(1, 6) i;
 CREATE TABLE ds (grp int, v int);
 INSERT INTO ds VALUES (0, 10), (0, 20), (1, 100), (1, 200);
 ANALYZE dk, ds;
}

teardown
{
 DROP TABLE dk;
 DROP TABLE ds;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1g	{ UPDATE dk SET grp = 1 WHERE id = 2; }
step s1v	{ UPDATE dk SET val = 1000 WHERE id = 3; }
step s1i	{ INSERT INTO ds VALUES (1, 900); }
step s1c	{ COMMIT; }

session s2
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s2u	{ UPDATE dk t SET val = (SELECT max(v) FROM ds WHERE ds.grp = t.grp) WHERE t.id <= 3; }
step s2w	{ UPDATE dk t SET val = -t.val WHERE t.val < (SELECT avg(v) FROM ds WHERE ds.grp = t.grp) AND t.id <= 4; }
step s2r	{ UPDATE dk t SET val = (SELECT count(*) FROM ds WHERE ds.grp = t.grp) WHERE t.id <= 3 RETURNING id, grp, val; }
step s2d	{ DELETE FROM dk t WHERE t.val < (SELECT max(v) FROM ds WHERE ds.grp = t.grp) AND t.id <= 4; }
step s2c	{ COMMIT; }

session s3
step s3sel	{ SELECT id, grp, val FROM dk ORDER BY id; }

permutation s1g s2u s1c s2c s3sel
permutation s1g s1i s2u s1c s2c s3sel
permutation s1v s2w s1c s2c s3sel
permutation s1g s2r s1c s2c s3sel
permutation s1g s1v s2d s1c s2c s3sel

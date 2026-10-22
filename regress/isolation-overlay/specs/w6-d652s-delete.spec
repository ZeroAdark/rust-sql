# D652-S1 DELETE (modifytable): a DELETE whose WHERE reads a correlated SubPlan over an aggregate inner meets rows s1
# changed. Its EvalPlanQual recheck evaluates the WHERE again over each newest version, the SubPlan with the arguments
# of that version: one that still passes is deleted (a changed val still below the max of its group, a changed pad),
# one that fails stays (a val above the max, a new grp whose max it is above), a deleted one is skipped. The redo read
# a write site of no cells there, so every rechecked row stayed.

setup
{
 CREATE TABLE dk (id int PRIMARY KEY, grp int, val int, pad text);
 INSERT INTO dk SELECT i, i % 2, i * 10, 'p' || i FROM generate_series(1, 8) i;
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
step s1p	{ UPDATE dk SET val = 11 WHERE id = 1; }
step s1q	{ UPDATE dk SET pad = 'q' WHERE id = 3; }
step s1v	{ UPDATE dk SET val = 1000 WHERE id = 5; }
step s1g	{ UPDATE dk SET grp = 0 WHERE id = 7; }
step s1x	{ DELETE FROM dk WHERE id = 3; }
step s1c	{ COMMIT; }

session s2
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s2d	{ DELETE FROM dk t WHERE t.val < (SELECT max(v) FROM ds WHERE ds.grp = t.grp) AND t.id <= 7; }
step s2r	{ DELETE FROM dk t WHERE t.val < (SELECT max(v) FROM ds WHERE ds.grp = t.grp) AND t.id <= 7 RETURNING id, val; }
step s2c	{ COMMIT; }

session s3
step s3sel	{ SELECT id, grp, val, pad FROM dk ORDER BY id; }

permutation s1p s1q s1v s1g s2d s1c s2c s3sel
permutation s1p s1x s2d s1c s2c s3sel
permutation s1p s1v s2r s1c s2c s3sel

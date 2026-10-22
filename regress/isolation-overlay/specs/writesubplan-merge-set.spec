# writesubplan (D1112 x D652, a MERGE action's rerun set): a SubPlan only the MERGE's actions read, correlated to the target
# over an aggregate of another table (a set rerun at each call), meets rows s1 changed or deleted. READ COMMITTED: s2's
# MERGE waits on s1's row locks; after s1 commits, EvalPlanQual re-runs the join over the newest version and the actions
# from the first: the WHEN qual's and the SET's SubPlans re-execute with the new grp (s1's 2) over ws_s whole (its scan is
# no PlanRowMark's: never the stored row), so rows 1 and 3 take the group 2 values; row 4, which s1 deleted, is no longer
# matched. s2d's first action deletes the rows whose new group has more than two rows. With s2 first, s1 waits.

setup
{
 CREATE TABLE ws_t (id int PRIMARY KEY, grp int NOT NULL, val int NOT NULL);
 INSERT INTO ws_t SELECT i, i % 3, i * 10 FROM generate_series(1, 9) i;
 CREATE TABLE ws_s (id int PRIMARY KEY, grp int NOT NULL, val int NOT NULL);
 INSERT INTO ws_s SELECT i, i % 3, i * 100 FROM generate_series(1, 8) i;
 ANALYZE ws_t, ws_s;
}

teardown
{
 DROP TABLE ws_t;
 DROP TABLE ws_s;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1u	{ UPDATE ws_t SET grp = 2, val = val + 1 WHERE id IN (1, 3); }
step s1d	{ DELETE FROM ws_t WHERE id = 4; }
step s1c	{ COMMIT; }

session s2
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s2m	{ MERGE INTO ws_t t USING ws_s s ON t.id = s.id WHEN MATCHED AND s.id <= 5 AND (SELECT count(*) FROM ws_s s2 WHERE s2.grp = t.grp AND s2.val > t.val) > 1 THEN UPDATE SET val = (SELECT max(s2.val) FROM ws_s s2 WHERE s2.grp = t.grp) + t.val WHEN MATCHED AND s.id <= 5 THEN UPDATE SET val = -1; }
step s2d	{ MERGE INTO ws_t t USING ws_s s ON t.id = s.id WHEN MATCHED AND s.id <= 5 AND (SELECT count(*) FROM ws_s s2 WHERE s2.grp = t.grp) > 2 THEN DELETE WHEN MATCHED AND s.id <= 5 THEN UPDATE SET val = (SELECT min(s2.val) FROM ws_s s2 WHERE s2.grp = t.grp); }
step s2c	{ COMMIT; }

session s3
step s3sel	{ SELECT id, grp, val FROM ws_t ORDER BY id; }

permutation s1u s1d s2m s1c s2c s3sel
permutation s1u s1d s2d s1c s2c s3sel
permutation s2m s1u s2c s1c s3sel
permutation s2d s1d s2c s1c s3sel

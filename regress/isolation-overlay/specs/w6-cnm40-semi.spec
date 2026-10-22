# CNM-40 (modifytable, M2): an UPDATE / DELETE whose IN / EXISTS sublink PostgreSQL plans as a Hash Semi Join meets a
# target row s1 changed. The semi join carries the inner row's REFERENCE mark (its ctid, from the first inner tuple
# that passed the join qual), so the EvalPlanQual recheck refetches that row and reruns the join over the target's
# newest version: a new id the inner row no longer matches, or a new val the join filter rejects, drops the row; a
# changed val the filter still passes is written over the newest version; a deleted row is skipped.

setup
{
 CREATE TABLE sk (id int PRIMARY KEY, grp int, val int);
 INSERT INTO sk SELECT i, i % 7, i * 10 FROM generate_series(1, 400) i;
 CREATE TABLE ss (id int, grp int, v int);
 INSERT INTO ss SELECT i % 300 + 1, i % 5, i FROM generate_series(1, 450) i;
 ANALYZE sk, ss;
}

teardown
{
 DROP TABLE sk;
 DROP TABLE ss;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1i	{ UPDATE sk SET id = 1003 WHERE id = 3; }
step s1v	{ UPDATE sk SET val = 200 WHERE id = 8; }
step s1w	{ UPDATE sk SET val = 5000 WHERE id = 18; }
step s1d	{ DELETE FROM sk WHERE id = 13; }
step s1c	{ COMMIT; }

session s2
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s2u	{ UPDATE sk SET val = -val WHERE id IN (SELECT id FROM ss WHERE grp = 2); }
step s2e	{ UPDATE sk t SET val = -t.val WHERE EXISTS (SELECT 1 FROM ss s WHERE s.id = t.id AND s.grp = 2 AND s.v > t.val); }
step s2d	{ DELETE FROM sk WHERE id IN (SELECT id FROM ss WHERE grp = 2 AND v < 40) RETURNING id, val; }
step s2c	{ COMMIT; }

session s3
step s3sel	{ SELECT id, val FROM sk WHERE id IN (3, 8, 13, 18, 23, 1003) ORDER BY id; }
step s3sum	{ SELECT count(*) FILTER (WHERE val < 0), sum(val) FROM sk; }

permutation s1i s2u s1c s2c s3sel s3sum
permutation s1v s1d s2u s1c s2c s3sel s3sum
permutation s1v s1w s2e s1c s2c s3sel s3sum
permutation s1i s1v s1d s2d s1c s2c s3sel s3sum

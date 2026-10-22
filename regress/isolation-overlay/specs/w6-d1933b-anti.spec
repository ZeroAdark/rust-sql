# D1933B anti (modifytable, M3): an UPDATE / DELETE whose NOT EXISTS sublink PostgreSQL plans as a Hash Anti Join meets
# a target row s1 changed. The anti join's rows carry the inner relation's mark ctid NULL, so EvalPlanQualFetchRowMark
# finds no inner row and the recheck reruns the anti join over an empty inner: the newest version passes it whatever
# the inner table holds; the target's own filter is evaluated again over it, and a deleted row is skipped.

setup
{
 CREATE TABLE ak (id int PRIMARY KEY, grp int, val int);
 INSERT INTO ak SELECT i, i % 7, i * 10 FROM generate_series(1, 400) i;
 CREATE TABLE asx (id int, grp int);
 INSERT INTO asx SELECT i % 300 + 1, i % 5 FROM generate_series(1, 450) i;
 ANALYZE ak, asx;
}

teardown
{
 DROP TABLE ak;
 DROP TABLE asx;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1v	{ UPDATE ak SET val = 1 WHERE id = 350; }
step s1f	{ UPDATE ak SET val = 3000 WHERE id = 334; }
step s1d	{ DELETE FROM ak WHERE id = 399; }
step s1c	{ COMMIT; }

session s2
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s2d	{ DELETE FROM ak t WHERE NOT EXISTS (SELECT 1 FROM asx s WHERE s.id = t.id); }
step s2u	{ UPDATE ak t SET val = -t.val WHERE NOT EXISTS (SELECT 1 FROM asx s WHERE s.id = t.id) AND t.val % 3 <> 0; }
step s2c	{ COMMIT; }

session s3
step s3sel	{ SELECT id, val FROM ak WHERE id IN (301, 334, 350, 399) ORDER BY id; }
step s3sum	{ SELECT count(*), count(*) FILTER (WHERE val < 0), sum(val) FROM ak; }

permutation s1v s1d s2d s1c s2c s3sel s3sum
permutation s1v s1f s2u s1c s2c s3sel s3sum

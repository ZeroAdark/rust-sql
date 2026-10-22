# D652-I (modifytable): the statement's own UPDATE with a correlated MULTIEXPR SubPlan and an InitPlan in SET rechecks in
# the IR. s2 waits on the parent rows s1 changed; after s1 commits, the recheck reads each row's newest version, keeps
# the InitPlan value the statement computed (s1's new tag row is not in it) and runs the SubPlan again under the
# statement's snapshot (s1's new child row is not in it); a row s1 flagged off fails the qual and stays. With s2 first,
# s1 waits; after a rollback of s1, s2 writes the old versions.

setup
{
 CREATE TABLE ip (id int PRIMARY KEY, lo int, n int, hi int, flag bool NOT NULL DEFAULT true, note text);
 INSERT INTO ip (id) SELECT g FROM generate_series(1, 60) g;
 CREATE TABLE ic (pid int NOT NULL, seq int NOT NULL);
 INSERT INTO ic SELECT 1 + (g % 50), g FROM generate_series(1, 300) g;
 CREATE TABLE it (weight int NOT NULL);
 INSERT INTO it SELECT g * 2 FROM generate_series(1, 30) g;
 ANALYZE ip, ic, it;
}

teardown
{
 DROP TABLE ip;
 DROP TABLE ic;
 DROP TABLE it;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1t	{ INSERT INTO it VALUES (999); INSERT INTO ic VALUES (3, 5000); }
step s1n	{ UPDATE ip SET note = 'touched' WHERE id IN (3, 7); }
step s1f	{ UPDATE ip SET flag = false WHERE id = 11; }
step s1c	{ COMMIT; }
step s1r	{ ROLLBACK; }

session s2
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s2u	{ UPDATE ip SET (lo, n) = (SELECT count(*), max(c.seq) FROM ic c WHERE c.pid = ip.id), hi = (SELECT max(t.weight) FROM it t) WHERE ip.id <= 15 AND ip.flag RETURNING id, lo, n, hi; }
step s2c	{ COMMIT; }

session s3
step s3sel	{ SELECT id, lo, n, hi, flag, note FROM ip WHERE id IN (1, 3, 7, 11, 15) ORDER BY id; }

permutation s1t s1n s1f s2u s1c s2c s3sel
permutation s2u s1n s2c s1c s3sel
permutation s1t s1n s1f s2u s1r s2c s3sel

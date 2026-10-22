# D1710 (modifytable, M3 rows ip-0181 / ip-0184 / ip-0187): a DELETE (no RETURNING) whose WHERE compares to an
# InitPlan, an uncorrelated scalar subquery PostgreSQL evaluates once per statement, meets target rows s1 changed. s2
# waits on s1's row locks; after s1 commits, EvalPlanQual rechecks the newest version of each row against the same
# InitPlan value (the statement's, not a fresh one: s1's new parent row would raise max(id) to 999). A row s1 updated
# in another column is deleted, a row whose pid s1 moved away is kept, a row s1 deleted is skipped, and a row s1 moved
# onto the InitPlan's value was not in s2's scan. With s2 first, s1's writes wait on s2's deletes; after a rollback of
# s1, s2 deletes from the old versions.

setup
{
 CREATE TABLE ipp (id int PRIMARY KEY, grp int NOT NULL);
 INSERT INTO ipp SELECT g, g % 9 FROM generate_series(1, 505) g;
 CREATE TABLE ipc (id int PRIMARY KEY, pid int, val int NOT NULL);
 INSERT INTO ipc SELECT g, (g % 505) + 1, g FROM generate_series(1, 5000) g;
 CREATE INDEX ipc_pid ON ipc (pid);
 ANALYZE ipp, ipc;
}

teardown
{
 DROP TABLE ipc;
 DROP TABLE ipp;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1p	{ INSERT INTO ipp VALUES (999, 4); }
step s1u	{ UPDATE ipc SET val = -val WHERE id IN (498, 1003); }
step s1m	{ UPDATE ipc SET pid = 1 WHERE id = 1508; }
step s1d	{ DELETE FROM ipc WHERE id = 2013; }
step s1n	{ UPDATE ipc SET pid = 499 WHERE id = 1; }
step s1c	{ COMMIT; }
step s1r	{ ROLLBACK; }

session s2
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s2d	{ DELETE FROM ipc WHERE pid = (SELECT max(id) FROM ipp WHERE grp = 4); }
step s2c	{ COMMIT; }

session s3
step s3sel	{ SELECT id, pid, val FROM ipc WHERE id IN (1, 498, 1003, 1508, 2013, 2518, 4538) ORDER BY id; }
step s3cnt	{ SELECT count(*), sum(id) FROM ipc WHERE pid IN (499, 999); }

permutation s1p s1u s1m s1d s1n s2d s1c s2c s3sel s3cnt
permutation s2d s1u s2c s1m s1c s3sel s3cnt
permutation s1p s1u s1d s2d s1r s2c s3sel s3cnt

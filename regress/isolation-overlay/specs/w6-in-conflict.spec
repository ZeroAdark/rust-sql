# IN-CONFLICT (modifytable, M3): a correlated SubPlan in an INSERT's ON CONFLICT DO UPDATE (a SET entry, the WHERE)
# meets a conflicting row s1 changed. The INSERT waits on s1's row lock; after s1 commits, ExecOnConflictUpdate finds
# the row updated and restarts from the arbiter check (READ COMMITTED), so the conflict arm evaluates the SubPlan over
# the newest version of the existing row (s1's cnt); a row s1 deleted no longer conflicts and is inserted. With s2
# first, s1's UPDATE waits on s2's.

setup
{
 CREATE TABLE icq (id int PRIMARY KEY, email text, cnt int NOT NULL DEFAULT 0, note text);
 INSERT INTO icq SELECT i, 'e' || i, i % 5, NULL FROM generate_series(1, 2000) i;
 CREATE TABLE ics (id int NOT NULL, cnt int NOT NULL, w int NOT NULL);
 INSERT INTO ics SELECT i, i % 7, i % 11 FROM generate_series(1, 700) i;
 ANALYZE icq, ics;
}

teardown
{
 DROP TABLE icq;
 DROP TABLE ics;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1u	{ UPDATE icq SET cnt = 4 WHERE id IN (5, 7); }
step s1d	{ DELETE FROM icq WHERE id = 6; }
step s1c	{ COMMIT; }

session s2
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s2i	{ INSERT INTO icq (id, email) VALUES (5, 'x') ON CONFLICT (id) DO UPDATE SET cnt = (SELECT sum(w) FROM ics WHERE ics.cnt = icq.cnt AND ics.id < 50) RETURNING id, cnt; }
step s2w	{ INSERT INTO icq (id, email) VALUES (6, 'y'), (7, 'z') ON CONFLICT (id) DO UPDATE SET note = 'w' || EXCLUDED.email WHERE EXISTS (SELECT 1 FROM ics WHERE ics.id BETWEEN icq.cnt + 6 AND icq.cnt + 6 AND ics.w > 9) RETURNING id, cnt, note; }
step s2c	{ COMMIT; }

session s3
step s3sel	{ SELECT id, email, cnt, note FROM icq WHERE id IN (5, 6, 7) ORDER BY id; }

permutation s1u s2i s1c s2c s3sel
permutation s1u s1d s2w s1c s2c s3sel
permutation s2i s1u s2c s1c s3sel
permutation s2w s1d s2c s1c s3sel

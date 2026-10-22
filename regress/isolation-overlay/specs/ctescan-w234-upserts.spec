# ctescan with#234 (D2644, D1687): two upserts in one statement, a writing CTE's INSERT ... ON CONFLICT DO UPDATE beside
# the statement's own, under concurrent writers (READ COMMITTED). s2w is regress with#234: the own conflict arm reads the
# CTE through a MULTIEXPR SubPlan, the CTE's upsert updates the row the own arm locked, and the own update skips it
# (updated by its own command). s1 holds the conflicting row first: s2 waits on it, then restarts from the arbiter check
# on the newest version (s1 updated it), on no row (s1 deleted it: the own row goes in and the CTE, run at the end,
# raises 21000), or on the old version (s1 rolled back). s2x: two targets of different widths, each upsert's DO UPDATE
# arm through its own writer, s1 holding the row of one or the other.

setup
{
 CREATE TABLE wz (k int UNIQUE, v text);
 INSERT INTO wz SELECT g, 'insert' FROM generate_series(0, 16) g;
 CREATE TABLE wy (k int PRIMARY KEY, g int NOT NULL, n int NOT NULL);
 INSERT INTO wy SELECT g, g % 3, g * 7 FROM generate_series(1, 10) g;
 ANALYZE wz, wy;
}

teardown
{
 DROP TABLE wz, wy;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1u	{ UPDATE wz SET v = 's1' WHERE k = 2; }
step s1d	{ DELETE FROM wz WHERE k = 2; }
step s1y	{ UPDATE wy SET n = 100 WHERE k = 2; }
step s1c	{ COMMIT; }
step s1r	{ ROLLBACK; }

session s2
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s2w	{
 WITH simpletup AS (SELECT 2 k, 'Green' v),
 upsert_cte AS (
   INSERT INTO wz VALUES (2, 'Blue') ON CONFLICT (k) DO
     UPDATE SET (k, v) = (SELECT k, v FROM simpletup WHERE simpletup.k = wz.k)
     RETURNING k, v)
 INSERT INTO wz VALUES (2, 'Red') ON CONFLICT (k) DO
 UPDATE SET (k, v) = (SELECT k, v FROM upsert_cte WHERE upsert_cte.k = wz.k)
 RETURNING k, v;
}
step s2x	{ WITH u AS (INSERT INTO wy VALUES (2, 0, 0) ON CONFLICT (k) DO UPDATE SET n = wy.n + 1 RETURNING k, n) INSERT INTO wz VALUES (2, 'main') ON CONFLICT (k) DO UPDATE SET v = wz.v || '+main' RETURNING k, v; }
step s2c	{ COMMIT; }

session s3
step s3sel	{ SELECT k, v FROM wz WHERE k BETWEEN 1 AND 3 ORDER BY k; }
step s3y	{ SELECT k, g, n FROM wy WHERE k = 2; }

permutation s1u s2w s1c s2c s3sel
permutation s1u s2w s1r s2c s3sel
permutation s1d s2w s1c s2c s3sel
permutation s1u s2x s1c s2c s3sel s3y
permutation s1y s2x s1c s2c s3sel s3y
permutation s1d s2x s1c s2c s3sel s3y

setup { SET lock_timeout = '20s'; }
# D3278 (CTE TWICE): the statement's own UPDATE / DELETE joined to a writing CTE through two CteScans, each a ROW_MARK_COPY
# mark carrying its row whole. When the own write meets a row a concurrent session updated, PostgreSQL's EvalPlanQual hands
# each CteScan its mark's stored row (EvalPlanQualFetchRowMark's copy leg) and never runs the CTE again, so the CTE's
# nextval runs once per CTE row (the sequence's last value), and the rows the recheck joins carry the n the statement
# stored. Concurrent update (commit and abort), join key move and delete; and a MERGE whose source joins a MATERIALIZED
# CTE to itself (both CteScans COPY-marked). Expected output is PostgreSQL 18.6's.

setup
{
 CREATE TABLE tp (id int PRIMARY KEY, grp int NOT NULL, w int NOT NULL);
 INSERT INTO tp SELECT g, g % 3, 0 FROM generate_series(1, 9) g;
 CREATE TABLE tc (id int PRIMARY KEY, pid int, val int NOT NULL);
 INSERT INTO tc SELECT g, (g - 1) % 9 + 1, g FROM generate_series(1, 18) g;
 CREATE SEQUENCE tsq;
}

teardown
{
 DROP TABLE tp, tc;
 DROP SEQUENCE tsq;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1u	{ UPDATE tc SET val = val * 100 WHERE id IN (1, 4); }
step s1k	{ UPDATE tc SET pid = 2 WHERE id = 1; }
step s1d	{ DELETE FROM tc WHERE id = 4; }
step s1c	{ COMMIT; }
step s1a	{ ROLLBACK; }

session s2
step s2w	{ WITH u AS (UPDATE tp SET w = w + 1 WHERE grp = 1 RETURNING id, nextval('tsq') AS n) UPDATE tc SET val = val + u1.n * 1000 + u2.n FROM u u1, u u2 WHERE tc.pid = u1.id AND u2.id = u1.id; }
step s2m	{ WITH u AS MATERIALIZED (SELECT id, grp FROM tp WHERE grp = 1) MERGE INTO tc t USING (SELECT a.id, b.grp FROM u a JOIN u b ON b.id = a.id) s ON t.pid = s.id WHEN MATCHED THEN UPDATE SET val = t.val + 1000 + s.grp; }
step s2d	{ WITH u AS (UPDATE tp SET w = w + 1 WHERE grp = 1 RETURNING id, nextval('tsq') AS n) DELETE FROM tc USING u u1, u u2 WHERE tc.pid = u1.id AND u2.id = u1.id AND tc.val < 50; }

session s3
step s3sel	{ SELECT * FROM tc ORDER BY id; }
step s3tp	{ SELECT id, w FROM tp ORDER BY id; }
step s3seq	{ SELECT last_value FROM tsq; }

permutation s1u s2w s1c s3sel s3tp s3seq
permutation s1u s2w s1a s3sel s3seq
permutation s1k s2w s1c s3sel s3seq
permutation s1d s2w s1c s3sel s3seq
permutation s1u s2d s1c s3sel s3tp s3seq
permutation s1u s2m s1c s3sel
permutation s1k s2m s1c s3sel

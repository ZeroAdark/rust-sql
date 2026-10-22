# D3379 POSTSAME (modifytable, ops/modifytable/post.rs): the statement's own UPDATE / DELETE beside a data-modifying CTE
# nothing reads, with no RETURNING list, writing the same relation. PostgreSQL runs the CTE at ExecPostprocessPlan, after
# the own write. When the own write meets a row s1 changed it waits and EvalPlanQual rechecks the newest version; the
# CTE, run after, reads the statement's snapshot, meets the row s1 updated, follows the update chain to the own command's
# version and skips it (TM_SelfModified). The forms: s1 commits or aborts an update of a row both write; s1 moves a key
# out of the own write's range; s1 deletes a row; s1 updates a row only the CTE writes (the CTE waits after the own write
# ended, then rechecks it); the CTE's DELETE; the CTE's INSERT ... ON CONFLICT DO NOTHING of a key the own write deletes
# (no conflict: the CTE runs after). Expected output is PostgreSQL 18.6's.

setup
{
 CREATE TABLE ps (k int PRIMARY KEY, v int NOT NULL);
 INSERT INTO ps SELECT g, g FROM generate_series(1, 6) g;
 ANALYZE ps;
}

teardown
{
 DROP TABLE ps;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1u	{ UPDATE ps SET v = v + 100 WHERE k = 1; }
step s1k	{ UPDATE ps SET k = 10 WHERE k = 1; }
step s1d	{ DELETE FROM ps WHERE k = 1; }
step s1o	{ UPDATE ps SET v = v + 100 WHERE k = 4; }
step s1c	{ COMMIT; }
step s1a	{ ABORT; }

session s2
step s2u	{ WITH c AS (UPDATE ps SET v = v * 10 WHERE k IN (1, 4)) UPDATE ps SET v = v + 1 WHERE k <= 2; }
step s2d	{ WITH c AS (DELETE FROM ps WHERE k IN (1, 4)) UPDATE ps SET v = v + 1 WHERE k <= 2; }
step s2i	{ WITH c AS (INSERT INTO ps VALUES (1, -1) ON CONFLICT (k) DO NOTHING) DELETE FROM ps WHERE k = 1; }

session s3
step s3s	{ SELECT k, v FROM ps ORDER BY k; }

permutation s1u s2u s1c s3s
permutation s1u s2u s1a s3s
permutation s1k s2u s1c s3s
permutation s1d s2u s1c s3s
permutation s1o s2u s1c s3s
permutation s1u s2d s1c s3s
permutation s1o s2d s1c s3s
permutation s1u s2i s1c s3s
permutation s1d s2i s1c s3s

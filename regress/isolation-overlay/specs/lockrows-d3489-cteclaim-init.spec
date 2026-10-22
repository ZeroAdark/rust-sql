# lockrows D3489 CTECLAIM-INIT: a job-queue claim through an InitPlan (`id = (SELECT .. LIMIT 1 FOR UPDATE)`) attached at
# a data-modifying CTE's ModifyTable, the CTE's RETURNING archived, logged or counted by the statement. s2's claim waits
# on the row s1 changed; once s1 commits, the claim's LockRows rechecks that row's newest version and the CTE's write,
# which read the row's old version under its statement snapshot, rechecks the newest version. PostgreSQL's EvalPlanQual
# initializes the CTE's ModifyTable afresh in its recheck EState, so that first recheck runs the claim's InitPlan again
# (its LockRows locking again, our own lock TM_Ok): the claim's sequence advances again (s2s).
#  s1v: row 5's data changed: claimed, the RETURNING lists show the newest data.
#  s1p: row 5's priority moved to 9: still claimed, but the write's own qual priority < 5 fails on the newest version.
#  s1d: row 5 deleted: the claim skips it and takes row 10.
#  s1k: s1 holds row 5 (FOR UPDATE): s2's SKIP LOCKED claim takes row 10 without waiting.
# Forms: the archive DELETE (s2d), with its own qual (s2q), under SKIP LOCKED (s2k), the sorted claim's UPDATE (s2u), a
# counted claim (s2n), and the claim whose quals call nextval (s2x, the sequence shows each run). Expected output is
# PostgreSQL 18.6's (the C oracle).
setup
{
  CREATE SEQUENCE sq;
  CREATE TABLE job (id int PRIMARY KEY, state text NOT NULL, priority int NOT NULL, data text) WITH (autovacuum_enabled = off);
  INSERT INTO job SELECT g, CASE WHEN g % 5 = 0 THEN 'retry' WHEN g % 7 = 0 THEN 'active' ELSE 'created' END, g % 4, 'd' || g FROM generate_series(1, 300) g;
  CREATE TABLE done (id int, data text) WITH (autovacuum_enabled = off);
  CREATE TABLE job_log (id int, data text) WITH (autovacuum_enabled = off);
  ANALYZE job;
}

teardown
{
  DROP TABLE job, done, job_log;
  DROP SEQUENCE sq;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1v	{ UPDATE job SET data = 'new' || id WHERE id IN (5, 20); }
step s1p	{ UPDATE job SET priority = 9 WHERE id IN (5, 20); }
step s1d	{ DELETE FROM job WHERE id = 5; }
step s1k	{ SELECT id FROM job WHERE id = 5 FOR UPDATE; }
step s1c	{ COMMIT; }

session s2
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s2e	{ EXPLAIN (COSTS OFF) WITH d AS (DELETE FROM job WHERE id = (SELECT id FROM job WHERE state = 'retry' ORDER BY id LIMIT 1 FOR UPDATE) RETURNING id, data) INSERT INTO done SELECT * FROM d; }
step s2d	{ WITH d AS (DELETE FROM job WHERE id = (SELECT id FROM job WHERE state = 'retry' ORDER BY id LIMIT 1 FOR UPDATE) RETURNING id, data) INSERT INTO done SELECT * FROM d RETURNING id, data; }
step s2q	{ WITH d AS (DELETE FROM job WHERE id = (SELECT id FROM job WHERE state = 'retry' ORDER BY id LIMIT 1 FOR UPDATE) AND priority < 5 RETURNING id, data) INSERT INTO done SELECT * FROM d RETURNING id, data; }
step s2k	{ WITH d AS (DELETE FROM job WHERE id = (SELECT id FROM job WHERE state = 'retry' ORDER BY id LIMIT 1 FOR UPDATE SKIP LOCKED) RETURNING id, data) INSERT INTO done SELECT * FROM d RETURNING id, data; }
step s2u	{ WITH u AS (UPDATE job SET state = 'active', data = data || '+' WHERE id = (SELECT id FROM job WHERE state = 'retry' ORDER BY priority, id LIMIT 1 FOR UPDATE) RETURNING id, data) INSERT INTO job_log SELECT * FROM u RETURNING id, data; }
step s2n	{ WITH d AS (DELETE FROM job WHERE id = (SELECT id FROM job WHERE state = 'retry' ORDER BY id LIMIT 1 FOR UPDATE) RETURNING id, data) SELECT count(*), min(id), max(data) FROM d; }
step s2x	{ WITH d AS (DELETE FROM job WHERE id = (SELECT id FROM job WHERE state = 'retry' AND nextval('sq') > 0 ORDER BY id LIMIT 1 FOR UPDATE) RETURNING id, data) INSERT INTO done SELECT * FROM d RETURNING id, data; }
step s2s	{ SELECT last_value, is_called FROM sq; }
step s2c	{ COMMIT; }

session s3
step s3r	{ SELECT id, state, priority, data FROM job WHERE id IN (5, 10, 15, 20) ORDER BY id; SELECT * FROM done ORDER BY id; SELECT * FROM job_log ORDER BY id; }

permutation s2e s1c s2c
permutation s1v s2d s1c s2c s3r
permutation s1p s2q s1c s2c s3r
permutation s1d s2d s1c s2c s3r
permutation s1k s2k s1c s2c s3r
permutation s1v s2u s1c s2c s3r
permutation s1v s2n s1c s2c s3r
permutation s1v s2x s1c s2s s2c s3r
permutation s2x s2s s1c s2c s3r

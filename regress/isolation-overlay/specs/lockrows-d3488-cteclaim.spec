# lockrows D3488 CTECLAIM: a job-queue claim inside a data-modifying CTE whose RETURNING the statement archives, logs or
# counts. s2's claim's LockRows waits on the row s1 changed; once s1 commits, LockRows rechecks that row's newest version
# under the subquery's quals and the CTE's write, which read the row's old version under its statement snapshot,
# rechecks the newest version against the subquery's COPY row (EvalPlanQual):
#  s1v: rows 20 and 40's data changed: both claimed, the RETURNING lists show the newest data.
#  s1p: rows 10 and 40's priority moved to 9: still claimed by the subquery (its quals hold), but the write's own qual
#  priority < 5 fails on the newest version, so the row is skipped by the write (still locked).
#  s1i: row 5 renumbered 1005: the subquery hands on 1005, which no old row the write scans matches: five rows written.
#  s1d: row 15 deleted: the subquery skips it and takes one more row.
#  s1k: s1 holds rows 5 and 10 (FOR UPDATE): s2's SKIP LOCKED claim takes the next rows without waiting.
# Forms: the archive DELETE in index order (s2a), with its own qual (s2q), under SKIP LOCKED (s2s), the sorted claim's
# UPDATE logged with its own qual (s2u), and a counted claim (s2n). Expected output is PostgreSQL 18.6's (the C oracle).
setup
{
  CREATE TABLE job (id int PRIMARY KEY, state text NOT NULL, priority int NOT NULL, data text) WITH (autovacuum_enabled = off);
  INSERT INTO job SELECT g, CASE WHEN g % 5 = 0 THEN 'retry' WHEN g % 7 = 0 THEN 'active' ELSE 'created' END, g % 4, 'd' || g FROM generate_series(1, 300) g;
  CREATE TABLE done (id int, data text) WITH (autovacuum_enabled = off);
  CREATE TABLE job_log (id int, data text) WITH (autovacuum_enabled = off);
  ANALYZE job;
}

teardown
{
  DROP TABLE job, done, job_log;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1v	{ UPDATE job SET data = 'new' || id WHERE id IN (20, 40); }
step s1p	{ UPDATE job SET priority = 9 WHERE id IN (10, 40); }
step s1i	{ UPDATE job SET id = 1005 WHERE id = 5; }
step s1d	{ DELETE FROM job WHERE id = 15; }
step s1k	{ SELECT id FROM job WHERE state = 'retry' ORDER BY id LIMIT 2 FOR UPDATE; }
step s1c	{ COMMIT; }

session s2
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s2e	{ EXPLAIN (COSTS OFF) WITH d AS (DELETE FROM job WHERE id IN (SELECT id FROM job WHERE state = 'retry' ORDER BY id LIMIT 6 FOR UPDATE) RETURNING id, data) INSERT INTO done SELECT * FROM d; }
step s2eu	{ EXPLAIN (COSTS OFF) WITH u AS (UPDATE job SET state = 'active' WHERE id IN (SELECT id FROM job WHERE state = 'retry' ORDER BY priority, id LIMIT 5 FOR UPDATE) AND priority < 5 RETURNING id, data) INSERT INTO job_log SELECT * FROM u; }
step s2a	{ WITH d AS (DELETE FROM job WHERE id IN (SELECT id FROM job WHERE state = 'retry' ORDER BY id LIMIT 6 FOR UPDATE) RETURNING id, data) INSERT INTO done SELECT * FROM d RETURNING id, data; }
step s2q	{ WITH d AS (DELETE FROM job WHERE id IN (SELECT id FROM job WHERE state = 'retry' ORDER BY id LIMIT 6 FOR UPDATE) AND priority < 5 RETURNING id, data) INSERT INTO done SELECT * FROM d RETURNING id, data; }
step s2s	{ WITH d AS (DELETE FROM job WHERE id IN (SELECT id FROM job WHERE state = 'retry' ORDER BY id LIMIT 4 FOR UPDATE SKIP LOCKED) RETURNING id, data) INSERT INTO done SELECT * FROM d RETURNING id, data; }
step s2u	{ WITH u AS (UPDATE job SET state = 'active' WHERE id IN (SELECT id FROM job WHERE state = 'retry' ORDER BY priority, id LIMIT 5 FOR UPDATE) AND priority < 5 RETURNING id, data) INSERT INTO job_log SELECT * FROM u RETURNING id, data; }
step s2n	{ WITH d AS (DELETE FROM job WHERE id IN (SELECT id FROM job WHERE state = 'retry' ORDER BY id LIMIT 6 FOR UPDATE) RETURNING id, data) SELECT count(*), min(id), max(id), string_agg(data, ',' ORDER BY id) FROM d; }
step s2c	{ COMMIT; }

session s3
step s3r	{ SELECT id, state, priority, data FROM job WHERE id IN (5, 10, 15, 20, 25, 30, 35, 40, 60, 80, 100, 1005) ORDER BY id; SELECT * FROM done ORDER BY id; SELECT * FROM job_log ORDER BY id; }

permutation s2e s2eu s1c s2c
permutation s1v s2a s1c s2c s3r
permutation s1p s2q s1c s2c s3r
permutation s1i s2a s1c s2c s3r
permutation s1d s2a s1c s2c s3r
permutation s1k s2s s1c s2c s3r
permutation s1v s2u s1c s2c s3r
permutation s1p s2u s1c s2c s3r
permutation s1v s2n s1c s2c s3r

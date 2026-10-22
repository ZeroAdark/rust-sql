# lockrows CTELOCK (lk-0062 / 0070, M3 lk-0101): the row lock inside a CTE that writes nothing, its store read whole by
# the statement's own write. WITH c AS (SELECT id FROM job .. ORDER BY prio, id LIMIT n FOR UPDATE [SKIP LOCKED]) INSERT
# INTO claims_log SELECT .. FROM c: the CTE's LockRows keeps its own row mark and rechecks under its own EvalPlanQual. A
# row another transaction holds is skipped (SKIP LOCKED) or waited for (FOR UPDATE); after the wait the newest version is
# rechecked against the CTE's quals, a row that no longer qualifies (moved out of 'ready', deleted) is passed over and
# the Limit takes the next one in index order. WITH l AS (SELECT id FROM acct WHERE owner = .. FOR NO KEY UPDATE /
# FOR UPDATE) UPDATE / DELETE acct .. FROM l: the CTE locks the newest version of each owner row (or drops one whose
# owner moved), then the write, whose scan saw the older version, rechecks the join against the newest target row and
# the CTE row it stored (ROW_MARK_COPY), writing the newest version.

setup
{
 CREATE TABLE job (id int PRIMARY KEY, queue text NOT NULL, prio int NOT NULL, state text NOT NULL, note text);
 CREATE INDEX job_ready ON job (prio, id) WHERE state = 'ready';
 INSERT INTO job SELECT i, CASE WHEN i % 3 = 0 THEN 'sms' ELSE 'mail' END, i % 5, CASE WHEN i % 4 = 0 THEN 'done' ELSE 'ready' END, 'n' || i FROM generate_series(1, 200) i;
 CREATE TABLE claims_log (id int, who text);
 CREATE TABLE acct (id int PRIMARY KEY, owner text NOT NULL, bal numeric(12,2) NOT NULL, ver int NOT NULL DEFAULT 0);
 INSERT INTO acct SELECT i, 'o' || (i % 10), (i * 13) % 997, 0 FROM generate_series(1, 60) i;
 CREATE INDEX acct_owner ON acct (owner);
 ANALYZE job; ANALYZE claims_log; ANALYZE acct;
}

teardown
{
 DROP TABLE job, claims_log, acct;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1n	{ UPDATE job SET note = 'held' WHERE id = 5; }
step s1d	{ UPDATE job SET state = 'done' WHERE id = 5; }
step s1x	{ DELETE FROM job WHERE id = 5; }
step s1o	{ UPDATE acct SET bal = bal + 1000 WHERE id = 16; }
step s1w	{ UPDATE acct SET owner = 'zz' WHERE id = 16; }
step s1r	{ DELETE FROM acct WHERE id = 16; }
step s1c	{ COMMIT; }
step s1a	{ ROLLBACK; }

session s2
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; SET enable_sort = off; }
step s2le	{ EXPLAIN (COSTS OFF) WITH c AS (SELECT id FROM job WHERE state = 'ready' ORDER BY prio, id LIMIT 3 FOR UPDATE) INSERT INTO claims_log SELECT id, 'w2' FROM c; }
step s2l	{ WITH c AS (SELECT id FROM job WHERE state = 'ready' ORDER BY prio, id LIMIT 3 FOR UPDATE) INSERT INTO claims_log SELECT id, 'w2' FROM c; }
step s2k	{ WITH c AS (SELECT id FROM job WHERE state = 'ready' ORDER BY prio, id LIMIT 3 FOR UPDATE SKIP LOCKED) INSERT INTO claims_log SELECT id, 'w2' FROM c; }
step s2ue	{ EXPLAIN (COSTS OFF) WITH l AS (SELECT id FROM acct WHERE owner = 'o6' FOR NO KEY UPDATE) UPDATE acct a SET ver = ver + 1, bal = bal + 1 FROM l WHERE a.id = l.id; }
step s2u	{ WITH l AS (SELECT id FROM acct WHERE owner = 'o6' FOR NO KEY UPDATE) UPDATE acct a SET ver = ver + 1, bal = bal + 1 FROM l WHERE a.id = l.id; }
step s2d	{ WITH l AS (SELECT id FROM acct WHERE owner = 'o6' FOR UPDATE) DELETE FROM acct a USING l WHERE a.id = l.id; }
step s2c	{ COMMIT; }

session s3
step s3j	{ SELECT id, who FROM claims_log ORDER BY id; }
step s3a	{ SELECT id, owner, bal, ver FROM acct WHERE id % 10 = 6 ORDER BY id; }

permutation s2le s1c s2c
permutation s1n s2l s1c s2c s3j
permutation s1d s2l s1c s2c s3j
permutation s1x s2l s1c s2c s3j
permutation s1n s2l s1a s2c s3j
permutation s1n s2k s1c s2c s3j
permutation s2ue s1c s2c
permutation s1o s2u s1c s2c s3a
permutation s1w s2u s1c s2c s3a
permutation s1r s2u s1c s2c s3a
permutation s1o s2u s1a s2c s3a
permutation s1o s2d s1c s2c s3a
permutation s1w s2d s1c s2c s3a

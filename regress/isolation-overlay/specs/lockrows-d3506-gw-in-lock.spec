# D3506: graphile-worker 0.16.6's get_job, the claim CTE j's filter probing a hashed SubPlan whose plan locks the
# available queues (`job_queue_id IN (SELECT id FROM gq WHERE is_available FOR UPDATE SKIP LOCKED)`), read by the
# data-modifying CTE q and the statement's UPDATE (READ COMMITTED). PostgreSQL builds the set at the claim's first
# probe, locking each available queue no other session holds. A claim that waits (FOR UPDATE, no SKIP LOCKED) on a job
# another session changed rechecks the newest version against a set its EvalPlanQual builds again, locking again: rows
# it holds come back TM_Ok, a row released since is locked now, a row still held is skipped. The scenarios: two
# workers at once; a held queue; a deleted job; an updated job; a job moved to a queue a third session holds (FOR NO
# KEY UPDATE, which the move's foreign key check passes); a job moved to a queue released before the recheck, or
# updated by its holder before it released it (made unavailable: the rebuild's own recheck of that queue skips it, its
# newest version's lock kept; still available: the rebuild locks its newest version, and q's UPDATE rechecks it). Each
# prints whether each queue's xmax is this transaction's own xid and a fourth session's pgrowlocks of the queues (no
# multixact and no other lock mode for a row locked twice), its SKIP LOCKED probe of them, and the jobs and queues
# claimed. Two plan forms: s2 with seq scans off
# (the claim by its index, both writes Nested Loops into the primary keys: the form PostgreSQL picks for
# graphile-worker's tables at size), s5 the planner's own over these few rows (Limit -> LockRows -> Sort -> Seq Scan,
# q a Nested Loop over the CTE Scan, the statement's UPDATE a Hash Join). The expected output is PostgreSQL 18.6's.
setup
{
  CREATE TABLE gq (id int PRIMARY KEY, queue_name text NOT NULL UNIQUE, locked_at timestamptz, locked_by text, is_available boolean GENERATED ALWAYS AS (locked_at IS NULL) STORED NOT NULL);
  CREATE TABLE gj (id int PRIMARY KEY, job_queue_id int REFERENCES gq (id), payload text NOT NULL DEFAULT 'p0', priority int NOT NULL, run_at timestamptz NOT NULL, attempts int NOT NULL DEFAULT 0, max_attempts int NOT NULL DEFAULT 25, locked_by text, locked_at timestamptz, is_available boolean GENERATED ALWAYS AS (locked_at IS NULL AND attempts < max_attempts) STORED NOT NULL);
  CREATE INDEX gj_main ON gj (priority, run_at) INCLUDE (id, job_queue_id) WHERE is_available = true;
  INSERT INTO gq (id, queue_name) VALUES (1, 'qa'), (2, 'qb'), (3, 'qc');
  INSERT INTO gj (id, job_queue_id, priority, run_at) VALUES (1, 1, 0, '2026-01-01 00:01'), (2, 2, 0, '2026-01-01 00:02'), (3, NULL, 0, '2026-01-01 00:03'), (4, 1, 1, '2026-01-01 00:04');
  ANALYZE gq; ANALYZE gj;
  CREATE EXTENSION pgrowlocks;
}
teardown { DROP TABLE gj, gq; DROP EXTENSION pgrowlocks; }

session s1
setup		{ SET enable_seqscan = off; BEGIN; }
step s1get	{ WITH j AS (SELECT gj.job_queue_id, gj.priority, gj.run_at, gj.id FROM gj WHERE gj.is_available = true AND run_at <= '2026-01-02' AND (gj.job_queue_id IS NULL OR gj.job_queue_id IN (SELECT id FROM gq WHERE gq.is_available = true FOR UPDATE SKIP LOCKED)) ORDER BY priority ASC, run_at ASC LIMIT 1 FOR UPDATE SKIP LOCKED), q AS (UPDATE gq SET locked_by = 'w1', locked_at = '2026-01-02' FROM j WHERE gq.id = j.job_queue_id) UPDATE gj SET attempts = gj.attempts + 1, locked_by = 'w1', locked_at = '2026-01-02' FROM j WHERE gj.id = j.id RETURNING gj.id, gj.job_queue_id, gj.attempts; }
step s1holdA	{ SELECT id FROM gq WHERE id = 1 FOR UPDATE; }
step s1upd	{ UPDATE gj SET payload = 'p1' WHERE id = 1; }
step s1del	{ DELETE FROM gj WHERE id = 1; }
step s1toB	{ UPDATE gj SET job_queue_id = 2 WHERE id = 1; }
step s1toC	{ UPDATE gj SET job_queue_id = 3 WHERE id = 1; }
step s1c	{ COMMIT; }

session s2
setup		{ SET enable_seqscan = off; }
step s2get	{ BEGIN; WITH j AS (SELECT gj.job_queue_id, gj.priority, gj.run_at, gj.id FROM gj WHERE gj.is_available = true AND run_at <= '2026-01-02' AND (gj.job_queue_id IS NULL OR gj.job_queue_id IN (SELECT id FROM gq WHERE gq.is_available = true FOR UPDATE SKIP LOCKED)) ORDER BY priority ASC, run_at ASC LIMIT 1 FOR UPDATE SKIP LOCKED), q AS (UPDATE gq SET locked_by = 'w2', locked_at = '2026-01-02' FROM j WHERE gq.id = j.job_queue_id) UPDATE gj SET attempts = gj.attempts + 1, locked_by = 'w2', locked_at = '2026-01-02' FROM j WHERE gj.id = j.id RETURNING gj.id, gj.job_queue_id, gj.attempts; }
step s2more	{ WITH j AS (SELECT gj.job_queue_id, gj.priority, gj.run_at, gj.id FROM gj WHERE gj.is_available = true AND run_at <= '2026-01-02' AND (gj.job_queue_id IS NULL OR gj.job_queue_id IN (SELECT id FROM gq WHERE gq.is_available = true FOR UPDATE SKIP LOCKED)) ORDER BY priority ASC, run_at ASC LIMIT 1 FOR UPDATE SKIP LOCKED), q AS (UPDATE gq SET locked_by = 'w2', locked_at = '2026-01-02' FROM j WHERE gq.id = j.job_queue_id) UPDATE gj SET attempts = gj.attempts + 1, locked_by = 'w2', locked_at = '2026-01-02' FROM j WHERE gj.id = j.id RETURNING gj.id, gj.job_queue_id, gj.attempts; }
step s2wait	{ BEGIN; WITH j AS (SELECT gj.job_queue_id, gj.priority, gj.run_at, gj.id FROM gj WHERE gj.is_available = true AND run_at <= '2026-01-02' AND (gj.job_queue_id IS NULL OR gj.job_queue_id IN (SELECT id FROM gq WHERE gq.is_available = true FOR UPDATE SKIP LOCKED)) ORDER BY priority ASC, run_at ASC LIMIT 1 FOR UPDATE), q AS (UPDATE gq SET locked_by = 'w2', locked_at = '2026-01-02' FROM j WHERE gq.id = j.job_queue_id) UPDATE gj SET attempts = gj.attempts + 1, locked_by = 'w2', locked_at = '2026-01-02' FROM j WHERE gj.id = j.id RETURNING gj.id, gj.job_queue_id, gj.attempts; }
step s2rows	{ SELECT id, locked_by, xmax::text::bigint = txid_current() AS mine FROM gq ORDER BY id; }
step s2c	{ COMMIT; }

session s3
step s3holdB	{ BEGIN; SELECT id FROM gq WHERE id = 2 FOR NO KEY UPDATE; }
step s3holdC	{ BEGIN; SELECT id FROM gq WHERE id = 3 FOR NO KEY UPDATE; }
step s3offB	{ UPDATE gq SET locked_at = '2026-01-01' WHERE id = 2; }
step s3markB	{ UPDATE gq SET locked_by = 'held' WHERE id = 2; }
step s3c	{ COMMIT; }

session s4
step s4free	{ SELECT id FROM gq ORDER BY id FOR UPDATE SKIP LOCKED; }
step s4locks	{ SELECT locked_row, multi, modes FROM pgrowlocks('gq') ORDER BY locked_row; }
step s4show	{ SELECT 'job' AS t, id, job_queue_id AS q, locked_by, attempts, payload FROM gj WHERE locked_by IS NOT NULL UNION ALL SELECT 'queue', id, NULL, locked_by, NULL, queue_name FROM gq WHERE locked_by IS NOT NULL ORDER BY 1, 2; }

session s5
step s5get	{ BEGIN; WITH j AS (SELECT gj.job_queue_id, gj.priority, gj.run_at, gj.id FROM gj WHERE gj.is_available = true AND run_at <= '2026-01-02' AND (gj.job_queue_id IS NULL OR gj.job_queue_id IN (SELECT id FROM gq WHERE gq.is_available = true FOR UPDATE SKIP LOCKED)) ORDER BY priority ASC, run_at ASC LIMIT 1 FOR UPDATE SKIP LOCKED), q AS (UPDATE gq SET locked_by = 'w5', locked_at = '2026-01-02' FROM j WHERE gq.id = j.job_queue_id) UPDATE gj SET attempts = gj.attempts + 1, locked_by = 'w5', locked_at = '2026-01-02' FROM j WHERE gj.id = j.id RETURNING gj.id, gj.job_queue_id, gj.attempts; }
step s5more	{ WITH j AS (SELECT gj.job_queue_id, gj.priority, gj.run_at, gj.id FROM gj WHERE gj.is_available = true AND run_at <= '2026-01-02' AND (gj.job_queue_id IS NULL OR gj.job_queue_id IN (SELECT id FROM gq WHERE gq.is_available = true FOR UPDATE SKIP LOCKED)) ORDER BY priority ASC, run_at ASC LIMIT 1 FOR UPDATE SKIP LOCKED), q AS (UPDATE gq SET locked_by = 'w5', locked_at = '2026-01-02' FROM j WHERE gq.id = j.job_queue_id) UPDATE gj SET attempts = gj.attempts + 1, locked_by = 'w5', locked_at = '2026-01-02' FROM j WHERE gj.id = j.id RETURNING gj.id, gj.job_queue_id, gj.attempts; }
step s5wait	{ BEGIN; WITH j AS (SELECT gj.job_queue_id, gj.priority, gj.run_at, gj.id FROM gj WHERE gj.is_available = true AND run_at <= '2026-01-02' AND (gj.job_queue_id IS NULL OR gj.job_queue_id IN (SELECT id FROM gq WHERE gq.is_available = true FOR UPDATE SKIP LOCKED)) ORDER BY priority ASC, run_at ASC LIMIT 1 FOR UPDATE), q AS (UPDATE gq SET locked_by = 'w5', locked_at = '2026-01-02' FROM j WHERE gq.id = j.job_queue_id) UPDATE gj SET attempts = gj.attempts + 1, locked_by = 'w5', locked_at = '2026-01-02' FROM j WHERE gj.id = j.id RETURNING gj.id, gj.job_queue_id, gj.attempts; }
step s5rows	{ SELECT id, locked_by, xmax::text::bigint = txid_current() AS mine FROM gq ORDER BY id; }
step s5c	{ COMMIT; }

permutation s1get s2get s2rows s4locks s4free s1c s2c s4show
permutation s1holdA s2get s1c s2more s2rows s4locks s2c s4show
permutation s1del s2wait s1c s2rows s4locks s2c s4show
permutation s1upd s2wait s1c s2rows s4locks s4free s2c s4show
permutation s3holdC s1toC s2wait s1c s3c s2rows s4locks s2c s4show
permutation s3holdB s1toB s2wait s3c s1c s2rows s4locks s4free s2c s4show
permutation s3holdB s1toB s2wait s3offB s3c s1c s2rows s4locks s2c s4show
permutation s3holdB s1toB s2wait s3markB s3c s1c s2rows s4locks s2c s4show
permutation s1get s5get s5rows s4locks s4free s1c s5c s4show
permutation s1holdA s5get s1c s5more s5rows s4locks s5c s4show
permutation s1del s5wait s1c s5rows s4locks s5c s4show
permutation s1upd s5wait s1c s5rows s4locks s4free s5c s4show
permutation s3holdC s1toC s5wait s1c s3c s5rows s4locks s5c s4show
permutation s3holdB s1toB s5wait s3c s1c s5rows s4locks s4free s5c s4show
permutation s3holdB s1toB s5wait s3offB s3c s1c s5rows s4locks s5c s4show
permutation s3holdB s1toB s5wait s3markB s3c s1c s5rows s4locks s5c s4show

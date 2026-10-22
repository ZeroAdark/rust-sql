# D3650 NEST-COPY: graphile-worker 0.16 / 0.18's get_job over ONE named queue (READ COMMITTED). Over these few rows
# PostgreSQL plans the claim CTE j as Limit -> LockRows -> Sort -> Seq Scan probing the hashed SubPlan whose plan locks
# the available queues, the data-modifying CTE q as a Nested Loop whose outer is the queue's own scan and whose inner is
# the CTE Scan of j (COPY-marked), its key a Join Filter, and the statement's UPDATE as a Hash Join (with two or more
# queues the CTE Scan drives q: lockrows-d3506-gw-in-lock). The scenarios: two workers at once (the second skips the
# held queue's jobs); a held queue; a deleted job; an updated job, rechecked. Each prints whether the queue's xmax is this
# transaction's own xid, a second session's pgrowlocks of the queue and its SKIP LOCKED probe, and the jobs and queues
# claimed. The expected output is PostgreSQL 18.6's.
setup
{
  CREATE TABLE gq (id int PRIMARY KEY, queue_name text NOT NULL UNIQUE, locked_at timestamptz, locked_by text, is_available boolean GENERATED ALWAYS AS (locked_at IS NULL) STORED NOT NULL);
  CREATE TABLE gj (id int PRIMARY KEY, job_queue_id int REFERENCES gq (id), payload text NOT NULL DEFAULT 'p0', priority int NOT NULL, run_at timestamptz NOT NULL, attempts int NOT NULL DEFAULT 0, max_attempts int NOT NULL DEFAULT 25, locked_by text, locked_at timestamptz, is_available boolean GENERATED ALWAYS AS (locked_at IS NULL AND attempts < max_attempts) STORED NOT NULL);
  CREATE INDEX gj_main ON gj (priority, run_at) INCLUDE (id, job_queue_id) WHERE is_available = true;
  INSERT INTO gq (id, queue_name) VALUES (1, 'qa');
  INSERT INTO gj (id, job_queue_id, priority, run_at) VALUES (1, 1, 0, '2026-01-01 00:01'), (2, NULL, 0, '2026-01-01 00:02'), (3, 1, 1, '2026-01-01 00:03');
  ANALYZE gq; ANALYZE gj;
  CREATE EXTENSION pgrowlocks;
}
teardown { DROP TABLE gj, gq; DROP EXTENSION pgrowlocks; }

session s1
setup		{ BEGIN; }
step s1get	{ WITH j AS (SELECT gj.job_queue_id, gj.priority, gj.run_at, gj.id FROM gj WHERE gj.is_available = true AND run_at <= '2026-01-02' AND (gj.job_queue_id IS NULL OR gj.job_queue_id IN (SELECT id FROM gq WHERE gq.is_available = true FOR UPDATE SKIP LOCKED)) ORDER BY priority ASC, run_at ASC LIMIT 1 FOR UPDATE SKIP LOCKED), q AS (UPDATE gq SET locked_by = 'w1', locked_at = '2026-01-02' FROM j WHERE gq.id = j.job_queue_id) UPDATE gj SET attempts = gj.attempts + 1, locked_by = 'w1', locked_at = '2026-01-02' FROM j WHERE gj.id = j.id RETURNING gj.id, gj.job_queue_id, gj.attempts; }
step s1holdA	{ SELECT id FROM gq WHERE id = 1 FOR UPDATE; }
step s1upd	{ UPDATE gj SET payload = 'p1' WHERE id = 1; }
step s1del	{ DELETE FROM gj WHERE id = 1; }
step s1c	{ COMMIT; }

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

permutation s1get s5get s5rows s4locks s4free s1c s5c s4show
permutation s1holdA s5get s1c s5more s5rows s4locks s5c s4show
permutation s1del s5wait s1c s5rows s4locks s5c s4show
permutation s1upd s5wait s1c s5rows s4locks s4free s5c s4show

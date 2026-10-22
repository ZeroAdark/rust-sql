# D3647 BUILT-COPY: graphile-worker 0.17+ / 0.18.0's batched get_jobs (localQueue, batchSize 5) over three queues
# (READ COMMITTED). PostgreSQL plans the claim CTE j_raw as Limit -> LockRows -> Sort -> Seq Scan probing the hashed
# SubPlan whose plan locks the available queues, the DISTINCT ON CTE j (one job per queue), the data-modifying CTE q as
# a Hash Join whose build is the queues' own scan (fewer queues than the batch) probed by the CTE Scan of j
# (COPY-marked), and the statement's UPDATE as a Hash Join over the CTE Scan of j. The scenarios: two workers at once
# (the second skips the held queues and their jobs); a worker that waits on a job another worker claims and commits
# (its claim's recheck finds the job taken and skips it, as PostgreSQL's); a queue renamed while a worker waits on it
# (the wait form locks the queues FOR UPDATE: q's UPDATE then finds the version its snapshot read updated and rechecks
# the newest one, its hash clause over j's stored row, and updates it); a deleted job. Each prints the jobs and queues
# claimed and a second session's pgrowlocks of the queues. The expected output is PostgreSQL 18.6's.
setup
{
  CREATE TABLE gq (id int PRIMARY KEY, queue_name text NOT NULL UNIQUE, locked_at timestamptz, locked_by text, is_available boolean GENERATED ALWAYS AS (locked_at IS NULL) STORED NOT NULL);
  CREATE TABLE gj (id int PRIMARY KEY, job_queue_id int REFERENCES gq (id), payload text NOT NULL DEFAULT 'p0', priority int NOT NULL, run_at timestamptz NOT NULL, attempts int NOT NULL DEFAULT 0, max_attempts int NOT NULL DEFAULT 25, locked_by text, locked_at timestamptz, is_available boolean GENERATED ALWAYS AS (locked_at IS NULL AND attempts < max_attempts) STORED NOT NULL);
  CREATE INDEX gj_main ON gj (priority, run_at) INCLUDE (id, job_queue_id) WHERE is_available = true;
  INSERT INTO gq (id, queue_name) VALUES (1, 'qa'), (2, 'qb'), (3, 'qc');
  INSERT INTO gj (id, job_queue_id, priority, run_at) SELECT g, CASE WHEN g % 4 = 0 THEN NULL ELSE 1 + g % 3 END, g % 2, timestamptz '2026-01-01' + g * interval '1 minute' FROM generate_series(1, 12) g;
  ANALYZE gq; ANALYZE gj;
  CREATE EXTENSION pgrowlocks;
}
teardown { DROP TABLE gj, gq; DROP EXTENSION pgrowlocks; }

session s1
setup		{ BEGIN; }
step s1get	{ WITH j_raw AS (SELECT gj.job_queue_id, gj.priority, gj.run_at, gj.id FROM gj WHERE gj.is_available = true AND run_at <= '2026-01-02' AND (gj.job_queue_id IS NULL OR gj.job_queue_id IN (SELECT id FROM gq WHERE gq.is_available = true FOR UPDATE SKIP LOCKED)) ORDER BY priority ASC, run_at ASC LIMIT 5 FOR UPDATE SKIP LOCKED), j AS (SELECT DISTINCT ON (j_raw.job_queue_id, CASE WHEN j_raw.job_queue_id IS NULL THEN j_raw.id END) j_raw.job_queue_id, j_raw.priority, j_raw.run_at, j_raw.id FROM j_raw ORDER BY j_raw.job_queue_id, CASE WHEN j_raw.job_queue_id IS NULL THEN j_raw.id END, j_raw.priority, j_raw.run_at), q AS (UPDATE gq SET locked_by = 'w1', locked_at = '2026-01-02' FROM j WHERE gq.id = j.job_queue_id) UPDATE gj SET attempts = gj.attempts + 1, locked_by = 'w1', locked_at = '2026-01-02' FROM j WHERE gj.id = j.id RETURNING gj.id, gj.job_queue_id, gj.attempts; }
step s1ren	{ UPDATE gq SET queue_name = 'qa2' WHERE id = 1; }
step s1del	{ DELETE FROM gj WHERE id = 2; }
step s1c	{ COMMIT; }

session s4
step s4locks	{ SELECT locked_row, multi, modes FROM pgrowlocks('gq') ORDER BY locked_row; }
step s4show	{ SELECT 'job' AS t, id, job_queue_id AS q, locked_by, attempts FROM gj WHERE locked_by IS NOT NULL UNION ALL SELECT 'queue', id, NULL, locked_by || ' ' || queue_name, NULL FROM gq WHERE locked_by IS NOT NULL ORDER BY 1, 2; }

session s5
step s5x	{ EXPLAIN (COSTS OFF) WITH j_raw AS (SELECT gj.job_queue_id, gj.priority, gj.run_at, gj.id FROM gj WHERE gj.is_available = true AND run_at <= '2026-01-02' AND (gj.job_queue_id IS NULL OR gj.job_queue_id IN (SELECT id FROM gq WHERE gq.is_available = true FOR UPDATE SKIP LOCKED)) ORDER BY priority ASC, run_at ASC LIMIT 5 FOR UPDATE SKIP LOCKED), j AS (SELECT DISTINCT ON (j_raw.job_queue_id, CASE WHEN j_raw.job_queue_id IS NULL THEN j_raw.id END) j_raw.job_queue_id, j_raw.priority, j_raw.run_at, j_raw.id FROM j_raw ORDER BY j_raw.job_queue_id, CASE WHEN j_raw.job_queue_id IS NULL THEN j_raw.id END, j_raw.priority, j_raw.run_at), q AS (UPDATE gq SET locked_by = 'w5', locked_at = '2026-01-02' FROM j WHERE gq.id = j.job_queue_id) UPDATE gj SET attempts = gj.attempts + 1, locked_by = 'w5', locked_at = '2026-01-02' FROM j WHERE gj.id = j.id RETURNING gj.id, gj.job_queue_id, gj.attempts; }
step s5get	{ BEGIN; WITH j_raw AS (SELECT gj.job_queue_id, gj.priority, gj.run_at, gj.id FROM gj WHERE gj.is_available = true AND run_at <= '2026-01-02' AND (gj.job_queue_id IS NULL OR gj.job_queue_id IN (SELECT id FROM gq WHERE gq.is_available = true FOR UPDATE SKIP LOCKED)) ORDER BY priority ASC, run_at ASC LIMIT 5 FOR UPDATE SKIP LOCKED), j AS (SELECT DISTINCT ON (j_raw.job_queue_id, CASE WHEN j_raw.job_queue_id IS NULL THEN j_raw.id END) j_raw.job_queue_id, j_raw.priority, j_raw.run_at, j_raw.id FROM j_raw ORDER BY j_raw.job_queue_id, CASE WHEN j_raw.job_queue_id IS NULL THEN j_raw.id END, j_raw.priority, j_raw.run_at), q AS (UPDATE gq SET locked_by = 'w5', locked_at = '2026-01-02' FROM j WHERE gq.id = j.job_queue_id) UPDATE gj SET attempts = gj.attempts + 1, locked_by = 'w5', locked_at = '2026-01-02' FROM j WHERE gj.id = j.id RETURNING gj.id, gj.job_queue_id, gj.attempts; }
step s5wait	{ BEGIN; WITH j_raw AS (SELECT gj.job_queue_id, gj.priority, gj.run_at, gj.id FROM gj WHERE gj.is_available = true AND run_at <= '2026-01-02' AND (gj.job_queue_id IS NULL OR gj.job_queue_id IN (SELECT id FROM gq WHERE gq.is_available = true FOR UPDATE SKIP LOCKED)) ORDER BY priority ASC, run_at ASC LIMIT 5 FOR UPDATE), j AS (SELECT DISTINCT ON (j_raw.job_queue_id, CASE WHEN j_raw.job_queue_id IS NULL THEN j_raw.id END) j_raw.job_queue_id, j_raw.priority, j_raw.run_at, j_raw.id FROM j_raw ORDER BY j_raw.job_queue_id, CASE WHEN j_raw.job_queue_id IS NULL THEN j_raw.id END, j_raw.priority, j_raw.run_at), q AS (UPDATE gq SET locked_by = 'w5', locked_at = '2026-01-02' FROM j WHERE gq.id = j.job_queue_id) UPDATE gj SET attempts = gj.attempts + 1, locked_by = 'w5', locked_at = '2026-01-02' FROM j WHERE gj.id = j.id RETURNING gj.id, gj.job_queue_id, gj.attempts; }
step s5waitq	{ BEGIN; WITH j_raw AS (SELECT gj.job_queue_id, gj.priority, gj.run_at, gj.id FROM gj WHERE gj.is_available = true AND run_at <= '2026-01-02' AND (gj.job_queue_id IS NULL OR gj.job_queue_id IN (SELECT id FROM gq WHERE gq.is_available = true FOR UPDATE)) ORDER BY priority ASC, run_at ASC LIMIT 5 FOR UPDATE SKIP LOCKED), j AS (SELECT DISTINCT ON (j_raw.job_queue_id, CASE WHEN j_raw.job_queue_id IS NULL THEN j_raw.id END) j_raw.job_queue_id, j_raw.priority, j_raw.run_at, j_raw.id FROM j_raw ORDER BY j_raw.job_queue_id, CASE WHEN j_raw.job_queue_id IS NULL THEN j_raw.id END, j_raw.priority, j_raw.run_at), q AS (UPDATE gq SET locked_by = 'w5', locked_at = '2026-01-02' FROM j WHERE gq.id = j.job_queue_id) UPDATE gj SET attempts = gj.attempts + 1, locked_by = 'w5', locked_at = '2026-01-02' FROM j WHERE gj.id = j.id RETURNING gj.id, gj.job_queue_id, gj.attempts; }
step s5c	{ COMMIT; }

permutation s5x s1get s5get s4locks s1c s5c s4show
permutation s1get s5wait s1c s4locks s5c s4show
permutation s1ren s5waitq s1c s4locks s5c s4show
permutation s1del s5wait s1c s5c s4show

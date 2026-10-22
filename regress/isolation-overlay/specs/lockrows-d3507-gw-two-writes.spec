# D3507: graphile-worker's get_job without the queue IN lock: the claim CTE j (Limit -> LockRows -> Index Scan) read by
# the data-modifying CTE q (UPDATE of the claimed job's queue) and the statement's UPDATE of the job (READ COMMITTED).
# Two workers claim at once (SKIP LOCKED: the second takes the next job); a claim that waits (FOR UPDATE, no SKIP LOCKED)
# on a job another session changed rechecks it: still available is claimed (its newest version, the statement's UPDATE
# rechecking too), made unavailable or deleted moves on to the next job; q waiting on a queue row another session
# changed rechecks that row. Three plan forms, one session each: s2 the planner's own over these few rows (Limit ->
# LockRows -> Sort, q a Nested Loop over the CTE Scan, the statement's UPDATE a Hash Join), s3 with nested loops off (both
# writes Hash Joins over the CTE Scan), s4 with seq scans off (the claim's index scan, both writes Nested Loops over the
# CTE Scan into the primary key: the form PostgreSQL picks for graphile-worker's tables at size). The expected output is
# PostgreSQL 18.6's.
setup
{
  CREATE TABLE gq (id int PRIMARY KEY, queue_name text NOT NULL, job_count int NOT NULL DEFAULT 0, locked_by text, locked_at timestamptz);
  CREATE TABLE gj (id int PRIMARY KEY, job_queue_id int REFERENCES gq (id), payload text NOT NULL DEFAULT 'p0', priority int NOT NULL, run_at timestamptz NOT NULL, attempts int NOT NULL DEFAULT 0, locked_by text, locked_at timestamptz, is_available boolean NOT NULL DEFAULT true);
  CREATE INDEX gj_main ON gj (priority, run_at) INCLUDE (id, job_queue_id) WHERE is_available = true;
  INSERT INTO gq (id, queue_name) VALUES (1, 'qa'), (2, 'qb');
  INSERT INTO gj (id, job_queue_id, priority, run_at) VALUES (1, 1, 0, '2026-01-01 00:01'), (2, NULL, 0, '2026-01-01 00:02'), (3, 2, 0, '2026-01-01 00:03'), (4, NULL, 1, '2026-01-01 00:04');
  ANALYZE gq; ANALYZE gj;
}
teardown { DROP TABLE gj, gq; }

session s1
setup		{ BEGIN; }
step s1get	{ WITH j AS (SELECT gj.job_queue_id, gj.id FROM gj WHERE gj.is_available AND run_at <= '2026-01-02' ORDER BY priority, run_at LIMIT 1 FOR UPDATE SKIP LOCKED), q AS (UPDATE gq SET locked_by = 'w1', locked_at = '2026-01-02' FROM j WHERE gq.id = j.job_queue_id) UPDATE gj SET attempts = gj.attempts + 1, locked_by = 'w1', locked_at = '2026-01-02' FROM j WHERE gj.id = j.id RETURNING gj.id, gj.job_queue_id, gj.payload; }
step s1upd	{ UPDATE gj SET payload = 'p1' WHERE id = 1; }
step s1off	{ UPDATE gj SET is_available = false WHERE id = 1; }
step s1del	{ DELETE FROM gj WHERE id = 1; }
step s1q	{ UPDATE gq SET job_count = job_count + 10 WHERE id = 1; }
step s1c	{ COMMIT; }

session s2
step s2get	{ WITH j AS (SELECT gj.job_queue_id, gj.id FROM gj WHERE gj.is_available AND run_at <= '2026-01-02' ORDER BY priority, run_at LIMIT 1 FOR UPDATE SKIP LOCKED), q AS (UPDATE gq SET locked_by = 'w2', locked_at = '2026-01-02' FROM j WHERE gq.id = j.job_queue_id) UPDATE gj SET attempts = gj.attempts + 1, locked_by = 'w2', locked_at = '2026-01-02' FROM j WHERE gj.id = j.id RETURNING gj.id, gj.job_queue_id, gj.payload; }
step s2wait	{ WITH j AS (SELECT gj.job_queue_id, gj.id FROM gj WHERE gj.is_available AND run_at <= '2026-01-02' ORDER BY priority, run_at LIMIT 1 FOR UPDATE), q AS (UPDATE gq SET locked_by = 'w2', locked_at = '2026-01-02' FROM j WHERE gq.id = j.job_queue_id) UPDATE gj SET attempts = gj.attempts + 1, locked_by = 'w2', locked_at = '2026-01-02' FROM j WHERE gj.id = j.id RETURNING gj.id, gj.job_queue_id, gj.payload; }
step s2show	{ SELECT 'job' AS t, id, locked_by, attempts::text AS n, payload AS x FROM gj WHERE locked_by IS NOT NULL UNION ALL SELECT 'queue', id, locked_by, job_count::text, queue_name FROM gq WHERE locked_by IS NOT NULL OR job_count > 0 ORDER BY 1, 2; }

session s3
setup		{ SET enable_nestloop = off; }
step s3get	{ WITH j AS (SELECT gj.job_queue_id, gj.id FROM gj WHERE gj.is_available AND run_at <= '2026-01-02' ORDER BY priority, run_at LIMIT 1 FOR UPDATE SKIP LOCKED), q AS (UPDATE gq SET locked_by = 'w3', locked_at = '2026-01-02' FROM j WHERE gq.id = j.job_queue_id) UPDATE gj SET attempts = gj.attempts + 1, locked_by = 'w3', locked_at = '2026-01-02' FROM j WHERE gj.id = j.id RETURNING gj.id, gj.job_queue_id, gj.payload; }
step s3wait	{ WITH j AS (SELECT gj.job_queue_id, gj.id FROM gj WHERE gj.is_available AND run_at <= '2026-01-02' ORDER BY priority, run_at LIMIT 1 FOR UPDATE), q AS (UPDATE gq SET locked_by = 'w3', locked_at = '2026-01-02' FROM j WHERE gq.id = j.job_queue_id) UPDATE gj SET attempts = gj.attempts + 1, locked_by = 'w3', locked_at = '2026-01-02' FROM j WHERE gj.id = j.id RETURNING gj.id, gj.job_queue_id, gj.payload; }
step s3show	{ SELECT 'job' AS t, id, locked_by, attempts::text AS n, payload AS x FROM gj WHERE locked_by IS NOT NULL UNION ALL SELECT 'queue', id, locked_by, job_count::text, queue_name FROM gq WHERE locked_by IS NOT NULL OR job_count > 0 ORDER BY 1, 2; }

session s4
setup		{ SET enable_seqscan = off; }
step s4get	{ WITH j AS (SELECT gj.job_queue_id, gj.id FROM gj WHERE gj.is_available AND run_at <= '2026-01-02' ORDER BY priority, run_at LIMIT 1 FOR UPDATE SKIP LOCKED), q AS (UPDATE gq SET locked_by = 'w4', locked_at = '2026-01-02' FROM j WHERE gq.id = j.job_queue_id) UPDATE gj SET attempts = gj.attempts + 1, locked_by = 'w4', locked_at = '2026-01-02' FROM j WHERE gj.id = j.id RETURNING gj.id, gj.job_queue_id, gj.payload; }
step s4wait	{ WITH j AS (SELECT gj.job_queue_id, gj.id FROM gj WHERE gj.is_available AND run_at <= '2026-01-02' ORDER BY priority, run_at LIMIT 1 FOR UPDATE), q AS (UPDATE gq SET locked_by = 'w4', locked_at = '2026-01-02' FROM j WHERE gq.id = j.job_queue_id) UPDATE gj SET attempts = gj.attempts + 1, locked_by = 'w4', locked_at = '2026-01-02' FROM j WHERE gj.id = j.id RETURNING gj.id, gj.job_queue_id, gj.payload; }
step s4show	{ SELECT 'job' AS t, id, locked_by, attempts::text AS n, payload AS x FROM gj WHERE locked_by IS NOT NULL UNION ALL SELECT 'queue', id, locked_by, job_count::text, queue_name FROM gq WHERE locked_by IS NOT NULL OR job_count > 0 ORDER BY 1, 2; }

permutation s1get s2get s1c s2show
permutation s1upd s2wait s1c s2show
permutation s1off s2wait s1c s2show
permutation s1del s2wait s1c s2show
permutation s1q s2get s1c s2show
permutation s1get s3get s1c s3show
permutation s1upd s3wait s1c s3show
permutation s1off s3wait s1c s3show
permutation s1del s3wait s1c s3show
permutation s1q s3get s1c s3show
permutation s1get s4get s1c s4show
permutation s1upd s4wait s1c s4show
permutation s1off s4wait s1c s4show
permutation s1del s4wait s1c s4show
permutation s1q s4get s1c s4show

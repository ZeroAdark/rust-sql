# D3502 FAMCTE (pg-boss v10's fetch): the claim CTE over a LIST-partitioned job table, `WITH next AS (SELECT id FROM job
# WHERE name = .. AND state < 'active' .. ORDER BY priority DESC, created_on, id LIMIT n FOR UPDATE [SKIP LOCKED]) UPDATE
# job j .. FROM next WHERE name = .. AND j.id = next.id`: Limit -> LockRows -> Sort -> the one pruned leaf's scan (or an
# Append of two leaves' scans), the store read whole by the UPDATE (a Hash Join over the CTE Scan). The CTE's LockRows
# keeps its own row mark and rechecks under its own EvalPlanQual (in the IR over the arm the row's tableoid names): a row
# another transaction holds is skipped (SKIP LOCKED) or waited for (FOR UPDATE); after the wait the newest version is
# rechecked against the CTE's quals, a row that no longer qualifies (made 'completed', deleted) is passed over and the
# Limit takes the next in sort order, one whose sort key changed but still qualifies is claimed in its newest version,
# and one moved to another partition raises 40001. The UPDATE's own recheck reads the claimed row from the CTE Scan's
# COPY mark. With a claim-order partial index (s2i's quals imply its predicate, the other steps' do not) the CTE is
# Limit -> LockRows -> the one leaf's Index Scan (no Sort): the same rechecks over the leaf's scan. Row 9 (leaf pb_job_q1, priority 1) is the second q1
# row in claim order; s1 holds it.

setup
{
 CREATE TYPE pb_state AS ENUM ('created', 'retry', 'active', 'completed', 'cancelled', 'failed');
 CREATE TABLE pb_job (id int NOT NULL, name text NOT NULL, priority int NOT NULL DEFAULT 0, data jsonb, state pb_state NOT NULL DEFAULT 'created', retry_count int NOT NULL DEFAULT 0, start_after timestamptz NOT NULL DEFAULT now() - interval '1 minute', started_on timestamptz, created_on timestamptz NOT NULL, PRIMARY KEY (name, id)) PARTITION BY LIST (name);
 CREATE TABLE pb_job_q1 PARTITION OF pb_job FOR VALUES IN ('q1');
 CREATE TABLE pb_job_q2 PARTITION OF pb_job FOR VALUES IN ('q2');
 CREATE TABLE pb_job_common PARTITION OF pb_job DEFAULT;
 INSERT INTO pb_job (id, name, priority, data, state, created_on) SELECT g, CASE g % 3 WHEN 0 THEN 'q1' WHEN 1 THEN 'q2' ELSE 'q3' END, g % 2, jsonb_build_object('n', g), CASE WHEN g % 7 = 0 THEN 'completed'::pb_state WHEN g % 11 = 0 THEN 'retry'::pb_state ELSE 'created'::pb_state END, timestamptz '2026-01-01 00:00+00' + g * interval '1 second' FROM generate_series(1, 120) g;
 CREATE INDEX pb_job_fetch ON pb_job (name, priority DESC, created_on, id) WHERE state < 'active' AND retry_count >= 0;
 ANALYZE pb_job;
}

teardown
{
 DROP TABLE pb_job;
 DROP TYPE pb_state;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1n	{ UPDATE pb_job SET data = jsonb_build_object('held', true) WHERE id = 9; }
step s1d	{ UPDATE pb_job SET state = 'completed' WHERE id = 9; }
step s1x	{ DELETE FROM pb_job WHERE id = 9; }
step s1p	{ UPDATE pb_job SET priority = 0 WHERE id = 9; }
step s1m	{ UPDATE pb_job SET name = 'q2' WHERE id = 9; }
step s1c	{ COMMIT; }
step s1a	{ ROLLBACK; }

session s2
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s2e	{ EXPLAIN (COSTS OFF) WITH next AS (SELECT id FROM pb_job WHERE name = 'q1' AND state < 'active' AND start_after < now() ORDER BY priority DESC, created_on, id LIMIT 3 FOR UPDATE) UPDATE pb_job j SET state = 'active', started_on = now(), retry_count = CASE WHEN started_on IS NOT NULL THEN retry_count + 1 ELSE retry_count END FROM next WHERE name = 'q1' AND j.id = next.id; }
step s2u	{ WITH next AS (SELECT id FROM pb_job WHERE name = 'q1' AND state < 'active' AND start_after < now() ORDER BY priority DESC, created_on, id LIMIT 3 FOR UPDATE) UPDATE pb_job j SET state = 'active', started_on = now(), retry_count = CASE WHEN started_on IS NOT NULL THEN retry_count + 1 ELSE retry_count END FROM next WHERE name = 'q1' AND j.id = next.id; }
step s2k	{ WITH next AS (SELECT id FROM pb_job WHERE name = 'q1' AND state < 'active' AND start_after < now() ORDER BY priority DESC, created_on, id LIMIT 3 FOR UPDATE SKIP LOCKED) UPDATE pb_job j SET state = 'active', started_on = now(), retry_count = CASE WHEN started_on IS NOT NULL THEN retry_count + 1 ELSE retry_count END FROM next WHERE name = 'q1' AND j.id = next.id; }
step s2we	{ EXPLAIN (COSTS OFF) WITH next AS (SELECT id, name FROM pb_job WHERE name = ANY (ARRAY['q1', 'q2']) AND state < 'active' ORDER BY priority DESC, created_on, id LIMIT 4 FOR UPDATE) UPDATE pb_job j SET state = 'active' FROM next WHERE j.name = next.name AND j.id = next.id; }
step s2w	{ WITH next AS (SELECT id, name FROM pb_job WHERE name = ANY (ARRAY['q1', 'q2']) AND state < 'active' ORDER BY priority DESC, created_on, id LIMIT 4 FOR UPDATE) UPDATE pb_job j SET state = 'active' FROM next WHERE j.name = next.name AND j.id = next.id; }
step s2x	{ WITH next AS (SELECT id FROM pb_job WHERE name = 'q1' AND state < 'active' ORDER BY priority DESC, created_on, id LIMIT 2 FOR UPDATE) DELETE FROM pb_job j USING next WHERE j.name = 'q1' AND j.id = next.id; }
step s2ie	{ SET LOCAL enable_seqscan = off; EXPLAIN (COSTS OFF) WITH next AS (SELECT id FROM pb_job WHERE name = 'q1' AND state < 'active' AND retry_count >= 0 AND start_after < now() ORDER BY priority DESC, created_on, id LIMIT 3 FOR UPDATE) UPDATE pb_job j SET state = 'active', started_on = now() FROM next WHERE name = 'q1' AND j.id = next.id; }
step s2i	{ SET LOCAL enable_seqscan = off; WITH next AS (SELECT id FROM pb_job WHERE name = 'q1' AND state < 'active' AND retry_count >= 0 AND start_after < now() ORDER BY priority DESC, created_on, id LIMIT 3 FOR UPDATE) UPDATE pb_job j SET state = 'active', started_on = now() FROM next WHERE name = 'q1' AND j.id = next.id; }
step s2c	{ COMMIT; }

session s3
step s3sel	{ SELECT tableoid::regclass AS leaf, id, priority, state, retry_count, data FROM pb_job WHERE id <= 16 ORDER BY id; }

permutation s2e s2c
permutation s2we s2c
permutation s1n s2u s1c s2c s3sel
permutation s1d s2u s1c s2c s3sel
permutation s1x s2u s1c s2c s3sel
permutation s1p s2u s1c s2c s3sel
permutation s1m s2u s1c s2c s3sel
permutation s1n s2u s1a s2c s3sel
permutation s1n s2k s1c s2c s3sel
permutation s1n s2w s1c s2c s3sel
permutation s1d s2w s1c s2c s3sel
permutation s1m s2w s1c s2c s3sel
permutation s1d s2x s1c s2c s3sel
permutation s2ie s2c
permutation s1n s2i s1c s2c s3sel
permutation s1d s2i s1c s2c s3sel
permutation s1p s2i s1c s2c s3sel
permutation s1m s2i s1c s2c s3sel

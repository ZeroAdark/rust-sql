# realapp-oban (Oban 2.24.1, the Basic engine): its producer's claim (fetch_jobs: WITH "subset" AS (SELECT id .. WHERE
# state = 'available' AND queue = $1 ORDER BY priority, scheduled_at, id LIMIT $2 FOR UPDATE SKIP LOCKED) UPDATE oban_jobs
# .. FROM "subset" .. RETURNING ..) between two producers and beside a cancel; an ack (complete_job) over a job a cancel
# moved; the stager (UPDATE .. FROM (SELECT .. ORDER BY scheduled_at DESC, id DESC LIMIT $3) AS s1) over a job another
# session rescheduled; the pruner (DELETE .. USING (SELECT .. LIMIT $4) AS s1) over a job a retry made available again;
# cancel_all over a job another session completed. The tables are never analyzed. Expected output is PostgreSQL 18.6's.
# Array and jsonb parameters are written ARRAY[..] / jsonb_build_object (a spec's SQL block ends at its first closing brace).
setup
{
  CREATE TYPE ob_state AS ENUM ('available', 'suspended', 'scheduled', 'executing', 'retryable', 'completed', 'discarded', 'cancelled');
  CREATE TABLE ob_jobs (id bigserial PRIMARY KEY, state ob_state NOT NULL DEFAULT 'available', queue text NOT NULL DEFAULT 'default', worker text NOT NULL, args jsonb NOT NULL DEFAULT jsonb_build_object(), errors jsonb[] NOT NULL DEFAULT ARRAY[]::jsonb[], attempt integer NOT NULL DEFAULT 0, max_attempts integer NOT NULL DEFAULT 20, inserted_at timestamp NOT NULL DEFAULT timezone('UTC', now()), scheduled_at timestamp NOT NULL DEFAULT timezone('UTC', now()), attempted_at timestamp, completed_at timestamp, attempted_by text[], discarded_at timestamp, priority integer NOT NULL DEFAULT 0, tags text[] DEFAULT ARRAY[]::text[], meta jsonb DEFAULT jsonb_build_object(), cancelled_at timestamp, CONSTRAINT attempt_range CHECK (attempt between 0 and max_attempts));
  CREATE INDEX ob_jobs_state_queue_priority_scheduled_at_id_index ON ob_jobs (state, queue, priority, scheduled_at, id);
  INSERT INTO ob_jobs (worker, args, queue, priority, scheduled_at, inserted_at, state, attempt, attempted_at, completed_at) VALUES
    ('W', jsonb_build_object('n', 1), 'default', 0, '2026-10-01 10:01', '2026-10-01 10:00', 'available', 0, NULL, NULL),
    ('W', jsonb_build_object('n', 2), 'default', 0, '2026-10-01 10:02', '2026-10-01 10:00', 'available', 0, NULL, NULL),
    ('W', jsonb_build_object('n', 3), 'default', 1, '2026-10-01 10:03', '2026-10-01 10:00', 'available', 0, NULL, NULL),
    ('W', jsonb_build_object('n', 4), 'default', 0, '2026-10-01 10:04', '2026-10-01 10:00', 'available', 0, NULL, NULL),
    ('W', jsonb_build_object('n', 5), 'default', 2, '2026-10-01 10:05', '2026-10-01 10:00', 'available', 0, NULL, NULL),
    ('W', jsonb_build_object('n', 6), 'mail', 0, '2026-10-01 10:06', '2026-10-01 10:00', 'executing', 1, '2026-10-01 10:30', NULL),
    ('W', jsonb_build_object('n', 7), 'mail', 0, '2026-10-01 10:07', '2026-10-01 10:00', 'scheduled', 0, NULL, NULL),
    ('W', jsonb_build_object('n', 8), 'mail', 0, '2026-10-01 10:08', '2026-10-01 10:00', 'scheduled', 0, NULL, NULL),
    ('W', jsonb_build_object('n', 9), 'mail', 0, '2026-10-01 10:09', '2026-10-01 10:00', 'completed', 1, '2026-10-01 10:20', '2026-10-01 10:21'),
    ('W', jsonb_build_object('n', 10), 'mail', 0, '2026-10-01 10:10', '2026-10-01 10:00', 'completed', 1, '2026-10-01 10:20', '2026-10-01 10:22'),
    ('W', jsonb_build_object('n', 11), 'events', 0, '2026-10-01 10:11', '2026-10-01 10:00', 'available', 0, NULL, NULL),
    ('W', jsonb_build_object('n', 12), 'events', 0, '2026-10-01 10:12', '2026-10-01 10:00', 'available', 0, NULL, NULL);
}
teardown { DROP TABLE ob_jobs; DROP TYPE ob_state; }

session s1
setup		{ BEGIN; }
step s1f	{ WITH "subset" AS (SELECT so0."id" AS "id" FROM ob_jobs AS so0 WHERE (so0."state" = 'available') AND (so0."queue" = 'default') ORDER BY so0."priority", so0."scheduled_at", so0."id" LIMIT 2 FOR UPDATE SKIP LOCKED) UPDATE ob_jobs AS o0 SET "state" = 'executing', "attempted_at" = '2026-10-01 10:40', "attempted_by" = ARRAY['node1', 'u1'], "attempt" = o0."attempt" + 1 FROM "subset" AS f1 WHERE (TRUE) AND (o0."id" = f1."id") AND (o0."attempt" < o0."max_attempts") RETURNING o0."id", o0."state", o0."attempt", o0."attempted_by"; }
step s1x	{ UPDATE ob_jobs AS o0 SET "state" = 'cancelled', "cancelled_at" = '2026-10-01 10:41' WHERE (o0."id" = 1) AND (o0."state" NOT IN ('cancelled','completed','discarded')); }
step s1k	{ UPDATE ob_jobs AS o0 SET "state" = 'cancelled', "cancelled_at" = '2026-10-01 10:41' WHERE (o0."id" = 6) AND (o0."state" NOT IN ('cancelled','completed','discarded')); }
step s1r	{ UPDATE ob_jobs SET scheduled_at = '2026-10-01 12:00' WHERE id = 8; }
step s1a	{ UPDATE ob_jobs AS o0 SET "state" = 'available', "max_attempts" = GREATEST(o0."max_attempts", o0."attempt" + 1), "scheduled_at" = '2026-10-01 10:42', "completed_at" = NULL, "cancelled_at" = NULL, "discarded_at" = NULL FROM (SELECT so0.id AS id, so0.queue AS queue, so0.state AS state FROM ob_jobs AS so0 WHERE (so0."id" = 9) AND (so0."state" NOT IN ('available','executing'))) AS s1 WHERE (o0."id" = s1."id") RETURNING s1."id", s1."queue", s1."state"; }
step s1d	{ UPDATE ob_jobs AS o0 SET "state" = 'completed', "completed_at" = '2026-10-01 10:43' WHERE (o0."id" = 11) AND (o0."state" = ANY(ARRAY['available']::ob_state[])); }
step s1c	{ COMMIT; }
teardown	{ ROLLBACK; }

session s2
setup		{ BEGIN; }
step s2x	{ EXPLAIN (COSTS OFF) WITH "subset" AS (SELECT so0."id" AS "id" FROM ob_jobs AS so0 WHERE (so0."state" = 'available') AND (so0."queue" = 'default') ORDER BY so0."priority", so0."scheduled_at", so0."id" LIMIT 3 FOR UPDATE SKIP LOCKED) UPDATE ob_jobs AS o0 SET "state" = 'executing', "attempted_at" = '2026-10-01 10:45', "attempted_by" = ARRAY['node2', 'u2'], "attempt" = o0."attempt" + 1 FROM "subset" AS f1 WHERE (TRUE) AND (o0."id" = f1."id") AND (o0."attempt" < o0."max_attempts") RETURNING o0."id", o0."state", o0."attempt", o0."attempted_by"; }
step s2f	{ WITH "subset" AS (SELECT so0."id" AS "id" FROM ob_jobs AS so0 WHERE (so0."state" = 'available') AND (so0."queue" = 'default') ORDER BY so0."priority", so0."scheduled_at", so0."id" LIMIT 3 FOR UPDATE SKIP LOCKED) UPDATE ob_jobs AS o0 SET "state" = 'executing', "attempted_at" = '2026-10-01 10:45', "attempted_by" = ARRAY['node2', 'u2'], "attempt" = o0."attempt" + 1 FROM "subset" AS f1 WHERE (TRUE) AND (o0."id" = f1."id") AND (o0."attempt" < o0."max_attempts") RETURNING o0."id", o0."state", o0."attempt", o0."attempted_by"; }
step s2k	{ UPDATE ob_jobs AS o0 SET "state" = 'completed', "completed_at" = '2026-10-01 10:46' WHERE (o0."id" = 6) AND (o0."state" = ANY(ARRAY['executing']::ob_state[])) AND (o0."attempted_at" = '2026-10-01 10:30'); }
step s2s	{ UPDATE ob_jobs AS o0 SET "state" = 'available' FROM (SELECT so0."id" AS "id", so0."state" AS "state" FROM ob_jobs AS so0 WHERE (so0."state" IN ('scheduled','retryable')) AND (so0."scheduled_at" <= '2026-10-01 11:00') ORDER BY so0."scheduled_at" DESC, so0."id" DESC LIMIT 5000) AS s1 WHERE (o0."id" = s1."id") RETURNING o0."id", o0."queue", s1."state"; }
step s2p	{ DELETE FROM ob_jobs AS o0 USING (SELECT so0."id" AS "id", so0."queue" AS "queue", so0."state" AS "state" FROM ob_jobs AS so0 WHERE ((((so0."state" = 'completed') AND (so0."scheduled_at" < '2026-10-01 11:00')) OR ((so0."state" = 'cancelled') AND (so0."cancelled_at" < '2026-10-01 11:00'))) OR ((so0."state" = 'discarded') AND (so0."discarded_at" < '2026-10-01 11:00'))) AND (NOT (so0."queue" IS NULL)) LIMIT 10000) AS s1 WHERE (o0."id" = s1."id") RETURNING s1."id", s1."queue", s1."state"; }
step s2a	{ UPDATE ob_jobs AS o0 SET "state" = 'cancelled', "cancelled_at" = '2026-10-01 10:47' FROM (SELECT so0.id AS id, so0.queue AS queue, so0.state AS state FROM ob_jobs AS so0 WHERE (so0."queue" = 'events') AND (so0."state" NOT IN ('cancelled','completed','discarded'))) AS s1 WHERE (o0."id" = s1."id") RETURNING s1."id", s1."queue", s1."state"; }
step s2r	{ SELECT id, state, queue, attempt, attempted_by, scheduled_at, completed_at, cancelled_at FROM ob_jobs ORDER BY id; }
step s2c	{ COMMIT; }
teardown	{ ROLLBACK; }

# two producers: the second skips the first's claimed rows; a producer beside a cancel of the first job (locked: skipped)
permutation s2x s1f s2f s1c s2c s2r
permutation s1x s2f s1c s2c s2r
# an ack after a cancel moved the job: the recheck reads cancelled, nothing completes
permutation s1k s2k s1c s2c s2r
# the stager after a reschedule of job 8: its subquery row is the stored one, the newest version staged as PostgreSQL does
permutation s1r s2s s1c s2c s2r
# the pruner after a retry of job 9: the recheck deletes the newest version (the subquery row is the stored one)
permutation s1a s2p s1c s2c s2r
# cancel_all of a queue after job 11 completed: the stored subquery row joins the newest version
permutation s1d s2a s1c s2c s2r

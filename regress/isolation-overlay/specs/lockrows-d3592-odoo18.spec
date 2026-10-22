# D3592: Odoo 18's cron claim, LockRows -> Sort -> Nested Loop Left Join (the cron's scan, its filter ORing a hashed
# SubPlan over the triggers; Subquery Scan of the inlined last-progress CTE, COPY-marked). Over a newer cron version
# (READ COMMITTED) PostgreSQL's EvalPlanQual reruns the nest over it: the trigger test probes a table built under the
# statement's snapshot, the Subquery Scan gets the stored progress row (none for a null-extended row). A cron
# rescheduled ahead with its triggers deleted meanwhile is still claimed (the snapshot's triggers) with its stored
# progress row (a progress row inserted meanwhile not seen); a cron deactivated gives no row; one null-extended stays
# so; SKIP LOCKED skips a locked cron. The expected output is PostgreSQL 18.6's.
setup
{
  CREATE TABLE ocron (id serial PRIMARY KEY, active bool, nextcall timestamp NOT NULL, priority int, failure_count int DEFAULT 0);
  CREATE TABLE otrig (id serial PRIMARY KEY, cron_id int, call_at timestamp);
  CREATE TABLE oprog (id serial PRIMARY KEY, cron_id int NOT NULL, remaining int DEFAULT 0, done int DEFAULT 0);
  INSERT INTO ocron (active, nextcall, priority) SELECT g % 5 <> 0, timestamp '2026-01-01' + g * interval '10 minutes', g % 10 FROM generate_series(1, 60) g;
  INSERT INTO otrig (cron_id, call_at) SELECT g % 60 + 1, timestamp '2026-01-01' + g * interval '3 minutes' FROM generate_series(1, 40) g;
  INSERT INTO oprog (cron_id, remaining, done) SELECT g % 50 + 1, g % 4, g % 3 FROM generate_series(1, 90) g;
  DELETE FROM oprog WHERE cron_id = 12;
  ANALYZE ocron, otrig, oprog;
}
teardown { DROP TABLE oprog, otrig, ocron; }

session s1
setup		{ BEGIN; }
step s1u	{ UPDATE ocron SET failure_count = 2 WHERE id = 21; INSERT INTO oprog (cron_id, remaining, done) VALUES (21, 9, 9); }
step s1t	{ UPDATE ocron SET nextcall = timestamp '2026-03-01' WHERE id = 21; DELETE FROM otrig WHERE cron_id = 21; INSERT INTO oprog (cron_id, remaining, done) VALUES (21, 9, 9); }
step s1d	{ UPDATE ocron SET active = false WHERE id = 21; }
step s1n	{ UPDATE ocron SET failure_count = 2 WHERE id = 12; INSERT INTO oprog (cron_id, remaining, done) VALUES (12, 9, 9); }
step s1k	{ SELECT id FROM ocron WHERE id = 21 FOR UPDATE; }
step s1c	{ COMMIT; }
teardown	{ ROLLBACK; }

session s2
step s2x	{ EXPLAIN (COSTS OFF) WITH lcp0 AS (SELECT id AS progress_id, cron_id, done, remaining FROM oprog WHERE cron_id = 21 ORDER BY id DESC LIMIT 1) SELECT ocron.id, ocron.nextcall, ocron.failure_count, lcp.progress_id, lcp.remaining FROM ocron LEFT JOIN lcp0 lcp ON lcp.cron_id = ocron.id WHERE ocron.active = true AND (ocron.nextcall <= timestamp '2026-01-01 03:00' OR EXISTS (SELECT cron_id FROM otrig WHERE call_at <= timestamp '2026-01-01 03:00' AND cron_id = ocron.id)) AND ocron.id = 21 ORDER BY priority FOR NO KEY UPDATE OF ocron; }
step s2a	{ WITH lcp0 AS (SELECT id AS progress_id, cron_id, done, remaining FROM oprog WHERE cron_id = 21 ORDER BY id DESC LIMIT 1) SELECT ocron.id, ocron.nextcall, ocron.failure_count, lcp.progress_id, lcp.remaining FROM ocron LEFT JOIN lcp0 lcp ON lcp.cron_id = ocron.id WHERE ocron.active = true AND (ocron.nextcall <= timestamp '2026-01-01 03:00' OR EXISTS (SELECT cron_id FROM otrig WHERE call_at <= timestamp '2026-01-01 03:00' AND cron_id = ocron.id)) AND ocron.id = 21 ORDER BY priority FOR NO KEY UPDATE OF ocron; }
step s2b	{ WITH lcp0 AS (SELECT id AS progress_id, cron_id, done, remaining FROM oprog WHERE cron_id = 12 ORDER BY id DESC LIMIT 1) SELECT ocron.id, ocron.failure_count, lcp.progress_id, lcp.remaining FROM ocron LEFT JOIN lcp0 lcp ON lcp.cron_id = ocron.id WHERE ocron.active = true AND (ocron.nextcall <= timestamp '2026-01-01 03:00' OR EXISTS (SELECT cron_id FROM otrig WHERE call_at <= timestamp '2026-01-01 03:00' AND cron_id = ocron.id)) AND ocron.id = 12 ORDER BY priority FOR NO KEY UPDATE OF ocron; }
step s2sk	{ WITH lcp0 AS (SELECT id AS progress_id, cron_id, done, remaining FROM oprog WHERE cron_id = 21 ORDER BY id DESC LIMIT 1) SELECT ocron.id, lcp.progress_id FROM ocron LEFT JOIN lcp0 lcp ON lcp.cron_id = ocron.id WHERE ocron.active = true AND (ocron.nextcall <= timestamp '2026-01-01 03:00' OR EXISTS (SELECT cron_id FROM otrig WHERE call_at <= timestamp '2026-01-01 03:00' AND cron_id = ocron.id)) AND ocron.id = 21 ORDER BY priority FOR NO KEY UPDATE OF ocron SKIP LOCKED; }

permutation s2x s1u s2a s1c
permutation s1t s2a s1c
permutation s1d s2a s1c
permutation s1n s2b s1c
permutation s1k s2sk s1c

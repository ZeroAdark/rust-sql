# D3569 REFCOPY: Odoo 17 / 18's cron claim, LockRows -> Sort -> Nested Loop Left Join (the cron's scan; Subquery Scan of
# the inlined last-progress CTE, COPY-marked). Over a newer version of the cron (READ COMMITTED) PostgreSQL's
# EvalPlanQual hands the Subquery Scan the stored progress row (none when the row was null-extended) and reruns the nest
# over the newest cron: a progress row inserted meanwhile is not seen (the stored one joins, or the row stays
# null-extended); a cron deactivated or rescheduled meanwhile fails its quals (no row); SKIP LOCKED skips a locked cron.
# The expected output is PostgreSQL 18.6's.
setup
{
  CREATE TABLE ccron (id serial PRIMARY KEY, cron_name text NOT NULL, active bool, nextcall timestamp NOT NULL, priority int, failure_count int DEFAULT 0);
  CREATE TABLE cprog (id serial PRIMARY KEY, cron_id int NOT NULL, remaining int DEFAULT 0, done int DEFAULT 0, timed_out_counter int DEFAULT 0);
  INSERT INTO ccron (cron_name, active, nextcall, priority) SELECT 'cron' || g, g % 5 <> 0, timestamp '2026-01-01' + g * interval '10 minutes', g % 10 FROM generate_series(1, 60) g;
  INSERT INTO cprog (cron_id, remaining, done) SELECT g % 50 + 1, g % 4, g % 3 FROM generate_series(1, 90) g;
  ANALYZE ccron, cprog;
}
teardown { DROP TABLE cprog, ccron; }

session s1
setup		{ BEGIN; }
step s1u	{ UPDATE ccron SET failure_count = failure_count + 1 WHERE id = 7; }
step s1p	{ UPDATE ccron SET failure_count = 3 WHERE id = 7; INSERT INTO cprog (cron_id, remaining, done) VALUES (7, 9, 9); }
step s1n	{ UPDATE ccron SET failure_count = 3 WHERE id = 57; INSERT INTO cprog (cron_id, remaining, done) VALUES (57, 9, 9); }
step s1d	{ UPDATE ccron SET active = false WHERE id = 7; }
step s1r	{ UPDATE ccron SET nextcall = timestamp '2026-02-01' WHERE id = 7; }
step s1k	{ SELECT id FROM ccron WHERE id = 7 FOR UPDATE; }
step s1c	{ COMMIT; }
teardown	{ ROLLBACK; }

session s2
step s2x	{ EXPLAIN (COSTS OFF) WITH lcp0 AS (SELECT id AS progress_id, cron_id, timed_out_counter, done, remaining FROM cprog WHERE cron_id = 7 ORDER BY id DESC LIMIT 1) SELECT ccron.id, ccron.failure_count, lcp.progress_id, lcp.remaining, lcp.done FROM ccron LEFT JOIN lcp0 lcp ON lcp.cron_id = ccron.id WHERE ccron.active = true AND ccron.nextcall <= timestamp '2026-01-02' AND ccron.id = 7 ORDER BY priority FOR NO KEY UPDATE OF ccron; }
step s2a	{ WITH lcp0 AS (SELECT id AS progress_id, cron_id, timed_out_counter, done, remaining FROM cprog WHERE cron_id = 7 ORDER BY id DESC LIMIT 1) SELECT ccron.id, ccron.failure_count, lcp.progress_id, lcp.remaining, lcp.done FROM ccron LEFT JOIN lcp0 lcp ON lcp.cron_id = ccron.id WHERE ccron.active = true AND ccron.nextcall <= timestamp '2026-01-02' AND ccron.id = 7 ORDER BY priority FOR NO KEY UPDATE OF ccron; }
step s2b	{ WITH lcp0 AS (SELECT id AS progress_id, cron_id, timed_out_counter, done, remaining FROM cprog WHERE cron_id = 57 ORDER BY id DESC LIMIT 1) SELECT ccron.id, ccron.failure_count, lcp.progress_id, lcp.remaining, lcp.done FROM ccron LEFT JOIN lcp0 lcp ON lcp.cron_id = ccron.id WHERE ccron.active = true AND ccron.nextcall <= timestamp '2026-01-02' AND ccron.id = 57 ORDER BY priority FOR NO KEY UPDATE OF ccron; }
step s2i	{ WITH lcp0 AS (SELECT id AS progress_id, cron_id, timed_out_counter, done, remaining FROM cprog WHERE cron_id = 7 ORDER BY id DESC LIMIT 1) SELECT ccron.id, ccron.failure_count, lcp.progress_id, lcp.remaining FROM ccron JOIN lcp0 lcp ON lcp.cron_id = ccron.id WHERE ccron.active = true AND ccron.id = 7 ORDER BY priority FOR NO KEY UPDATE OF ccron; }
step s2sk	{ WITH lcp0 AS (SELECT id AS progress_id, cron_id, timed_out_counter, done, remaining FROM cprog WHERE cron_id = 7 ORDER BY id DESC LIMIT 1) SELECT ccron.id, lcp.progress_id FROM ccron LEFT JOIN lcp0 lcp ON lcp.cron_id = ccron.id WHERE ccron.active = true AND ccron.nextcall <= timestamp '2026-01-02' AND ccron.id = 7 ORDER BY priority FOR NO KEY UPDATE OF ccron SKIP LOCKED; }

permutation s2x s1u s2a s1c
permutation s1p s2a s1c
permutation s1n s2b s1c
permutation s1d s2a s1c
permutation s1r s2a s1c
permutation s1p s2i s1c
permutation s1k s2sk s1c

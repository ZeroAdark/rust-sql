# D3568 HASHSET: Odoo's cron claim, LockRows -> Sort -> the cron's scan whose filter ORs a hashed SubPlan over the
# triggers. Over a newer version of the cron (READ COMMITTED) PostgreSQL's EvalPlanQual reruns the scan's filter over it,
# its hashed SubPlan's table built again under the statement's snapshot (a trigger committed meanwhile not seen): a cron
# rescheduled ahead stays claimed while a trigger it had is visible; one rescheduled with its triggers deleted meanwhile
# still is (the snapshot's triggers); one deactivated is not; SKIP LOCKED skips a locked cron. The expected output is
# PostgreSQL 18.6's.
setup
{
  CREATE TABLE hcron (id serial PRIMARY KEY, active bool, numbercall int, nextcall timestamp NOT NULL, priority int, failure_count int DEFAULT 0);
  CREATE TABLE htrig (id serial PRIMARY KEY, cron_id int, call_at timestamp);
  INSERT INTO hcron (active, numbercall, nextcall, priority) SELECT g % 5 <> 0, -1, timestamp '2026-01-01' + g * interval '10 minutes', g % 10 FROM generate_series(1, 60) g;
  INSERT INTO htrig (cron_id, call_at) SELECT g % 60 + 1, timestamp '2026-01-01' + g * interval '3 minutes' FROM generate_series(1, 40) g;
  ANALYZE hcron, htrig;
}
teardown { DROP TABLE htrig, hcron; }

session s1
setup		{ BEGIN; }
step s1u	{ UPDATE hcron SET failure_count = 1 WHERE id = 21; }
step s1a	{ UPDATE hcron SET nextcall = timestamp '2026-03-01' WHERE id = 21; }
step s1t	{ UPDATE hcron SET nextcall = timestamp '2026-03-01' WHERE id = 21; DELETE FROM htrig WHERE cron_id = 21; }
step s1n	{ UPDATE hcron SET nextcall = timestamp '2026-03-01' WHERE id = 7; INSERT INTO htrig (cron_id, call_at) VALUES (7, timestamp '2026-01-01'); }
step s1d	{ UPDATE hcron SET active = false WHERE id = 21; }
step s1k	{ SELECT id FROM hcron WHERE id = 21 FOR UPDATE; }
step s1c	{ COMMIT; }
teardown	{ ROLLBACK; }

session s2
step s2x	{ EXPLAIN (COSTS OFF) SELECT id, nextcall, failure_count FROM hcron WHERE active = true AND numbercall != 0 AND (nextcall <= timestamp '2026-01-01 03:00' OR EXISTS (SELECT cron_id FROM htrig WHERE call_at <= timestamp '2026-01-01 03:00' AND cron_id = hcron.id)) AND id = 21 ORDER BY priority FOR NO KEY UPDATE; }
step s2a	{ SELECT id, nextcall, failure_count FROM hcron WHERE active = true AND numbercall != 0 AND (nextcall <= timestamp '2026-01-01 03:00' OR EXISTS (SELECT cron_id FROM htrig WHERE call_at <= timestamp '2026-01-01 03:00' AND cron_id = hcron.id)) AND id = 21 ORDER BY priority FOR NO KEY UPDATE; }
step s2b	{ SELECT id, nextcall FROM hcron WHERE active = true AND numbercall != 0 AND (nextcall <= timestamp '2026-01-01 03:00' OR EXISTS (SELECT cron_id FROM htrig WHERE call_at <= timestamp '2026-01-01 03:00' AND cron_id = hcron.id)) AND id = 7 ORDER BY priority FOR NO KEY UPDATE; }
step s2sk	{ SELECT id FROM hcron WHERE active = true AND numbercall != 0 AND (nextcall <= timestamp '2026-01-01 03:00' OR EXISTS (SELECT cron_id FROM htrig WHERE call_at <= timestamp '2026-01-01 03:00' AND cron_id = hcron.id)) AND id = 21 ORDER BY priority FOR NO KEY UPDATE SKIP LOCKED; }

permutation s2x s1u s2a s1c
permutation s1a s2a s1c
permutation s1t s2a s1c
permutation s1n s2b s1c
permutation s1d s2a s1c
permutation s1k s2sk s1c

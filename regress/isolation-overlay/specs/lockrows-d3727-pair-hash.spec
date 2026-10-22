# D3727 PAIRHASH (gexplain; realapp-django): a LIMIT above LockRows over PostgreSQL's own Hash Join whose two relations
# are both locked (Django's select_for_update() then get() across a many-to-many relation): Limit -> LockRows -> Hash Join
# (Seq Scan d3727s, Hash <- Bitmap Heap Scan d3727ms). ExecLockRows locks d3727s's row, then d3727ms's (PlanRowMark
# order); when s1 changed either and committed (READ COMMITTED), EvalPlanQual reruns the Hash Join over the two test
# tuples: a row whose new version fails its side's qual is skipped (the locks taken stay), one that still passes is
# returned in its new version, and the Limit counts only rows that pass. Each s2 step reads back the rows its transaction
# locked (age(xmax) = 0). Expected output is PostgreSQL 18.6's.
setup
{
  CREATE TABLE d3727s (id serial PRIMARY KEY, field int NOT NULL, created timestamptz NOT NULL DEFAULT '2026-01-01 00:00:00+00');
  CREATE TABLE d3727m (id serial PRIMARY KEY);
  CREATE TABLE d3727ms (id serial PRIMARY KEY, m_id int NOT NULL REFERENCES d3727m (id), s_id int NOT NULL REFERENCES d3727s (id), UNIQUE (m_id, s_id));
  CREATE INDEX d3727ms_m ON d3727ms (m_id);
  CREATE INDEX d3727ms_s ON d3727ms (s_id);
  INSERT INTO d3727s (field) SELECT g % 4 FROM generate_series(1, 12) g;
  INSERT INTO d3727m SELECT FROM generate_series(1, 3);
  INSERT INTO d3727ms (m_id, s_id) VALUES (1, 1), (1, 2), (1, 6), (2, 2), (2, 3), (3, 10);
}
teardown { DROP TABLE d3727ms, d3727m, d3727s; }

session s1
setup		{ BEGIN; }
step s1f	{ UPDATE d3727s SET field = 5 WHERE id = 2; }
step s1m	{ UPDATE d3727ms SET m_id = 3 WHERE id = 2; }
step s1k	{ UPDATE d3727s SET created = '2026-02-01 00:00:00+00' WHERE id = 6; }
step s1c	{ COMMIT; }
teardown	{ ROLLBACK; }

session s2
setup		{ SET enable_nestloop = off; SET enable_mergejoin = off; BEGIN; }
step s2x	{ EXPLAIN (COSTS OFF) SELECT s.id, s.field, s.created FROM d3727s s JOIN d3727ms ms ON s.id = ms.s_id WHERE ms.m_id = 1 AND s.field = 2 LIMIT 21 FOR UPDATE; }
step s2l	{ SELECT s.id, s.field, s.created FROM d3727s s JOIN d3727ms ms ON s.id = ms.s_id WHERE ms.m_id = 1 AND s.field = 2 LIMIT 21 FOR UPDATE; }
step s2one	{ SELECT s.id, s.field, s.created FROM d3727s s JOIN d3727ms ms ON s.id = ms.s_id WHERE ms.m_id = 1 AND s.field = 2 LIMIT 1 FOR UPDATE; }
step s2r	{ SELECT 's' AS t, id FROM d3727s WHERE age(xmax) = 0 UNION ALL SELECT 'ms', id FROM d3727ms WHERE age(xmax) = 0 ORDER BY 1, 2; }
step s2c	{ COMMIT; }
teardown	{ ROLLBACK; }

# the plan
permutation s2x s1c s2c
# the outer row's new version fails its qual: skipped, its lock kept; the other row returned
permutation s1f s2l s1c s2r s2c
# the link row's new version fails its qual: skipped (the outer row stays locked), the other row returned
permutation s1m s2l s1c s2r s2c
# a new version that still passes: returned as it is now
permutation s1k s2l s1c s2r s2c
# LIMIT 1: the skipped row is not counted, so the next one is returned
permutation s1f s2one s1c s2r s2c

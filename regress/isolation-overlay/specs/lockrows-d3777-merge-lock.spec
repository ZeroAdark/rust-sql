# D3777 PAIRMERGE / LOCKMERGE / MERGETAIL (gexplain; realapp-django): a LIMIT above LockRows over PostgreSQL's Merge Join
# (Django's select_for_update() then get() across a many-to-many relation with hash joins and nested loops off):
# Limit -> LockRows -> Merge Join (Sort -> Seq Scan d3777s, Sort -> Bitmap Heap Scan d3777ms). When s1 changed a locked
# row and committed (READ COMMITTED), EvalPlanQual reruns the Merge Join over the test tuples, outer first: a row whose
# new version fails its side's qual or the merge clause is skipped (the locks taken stay), one that still passes is
# returned in its new version, and the Limit counts only rows that pass. s2l locks both relations, s2i the link table
# (the outer a REFERENCE mark), s2o the outer relation (the link table a REFERENCE mark). Each s2 step reads back the
# rows its transaction locked (age(xmax) = 0). s2li / s2ii: the same over Index Scans in the clauses' order (LIMIT 4 of
# the six joined rows). Expected output is PostgreSQL 18.6's.
setup
{
  CREATE TABLE d3777s (id serial PRIMARY KEY, field int NOT NULL, created timestamptz NOT NULL DEFAULT '2026-01-01 00:00:00+00');
  CREATE TABLE d3777m (id serial PRIMARY KEY);
  CREATE TABLE d3777ms (id serial PRIMARY KEY, m_id int NOT NULL REFERENCES d3777m (id), s_id int NOT NULL REFERENCES d3777s (id), UNIQUE (m_id, s_id));
  CREATE INDEX d3777ms_m ON d3777ms (m_id);
  CREATE INDEX d3777ms_s ON d3777ms (s_id);
  INSERT INTO d3777s (field) SELECT g % 4 FROM generate_series(1, 12) g;
  INSERT INTO d3777m SELECT FROM generate_series(1, 3);
  INSERT INTO d3777ms (m_id, s_id) VALUES (1, 1), (1, 2), (1, 6), (2, 2), (2, 3), (3, 10);
}
teardown { DROP TABLE d3777ms, d3777m, d3777s; }

session s1
setup		{ BEGIN; }
step s1f	{ UPDATE d3777s SET field = 0 WHERE id = 2; }
step s1g	{ UPDATE d3777s SET field = 3 WHERE id = 6; }
step s1m	{ UPDATE d3777ms SET m_id = 3 WHERE id = 2; }
step s1j	{ UPDATE d3777ms SET s_id = 7 WHERE id = 2; }
step s1n	{ UPDATE d3777ms SET m_id = m_id WHERE id = 2; }
step s1c	{ COMMIT; }
teardown	{ ROLLBACK; }

session s2
setup		{ SET enable_hashjoin = off; SET enable_nestloop = off; BEGIN; }
step s2x	{ EXPLAIN (COSTS OFF) SELECT s.id, s.field, ms.id AS ms FROM d3777s s JOIN d3777ms ms ON s.id = ms.s_id WHERE ms.m_id = 1 AND s.field >= 2 LIMIT 21 FOR UPDATE; }
step s2l	{ SELECT s.id, s.field, ms.id AS ms FROM d3777s s JOIN d3777ms ms ON s.id = ms.s_id WHERE ms.m_id = 1 AND s.field >= 2 LIMIT 21 FOR UPDATE; }
step s2one	{ SELECT s.id, s.field, ms.id AS ms FROM d3777s s JOIN d3777ms ms ON s.id = ms.s_id WHERE ms.m_id = 1 AND s.field >= 2 LIMIT 1 FOR UPDATE; }
step s2i	{ SELECT s.id, s.field, ms.id AS ms FROM d3777s s JOIN d3777ms ms ON s.id = ms.s_id WHERE ms.m_id = 1 AND s.field >= 2 LIMIT 21 FOR UPDATE OF ms; }
step s2o	{ SELECT s.id, s.field, ms.id AS ms FROM d3777s s JOIN d3777ms ms ON s.id = ms.s_id WHERE ms.m_id = 1 AND s.field >= 2 LIMIT 21 FOR UPDATE OF s; }
step s2xi	{ EXPLAIN (COSTS OFF) SELECT s.id, s.field, ms.id AS ms FROM d3777s s JOIN d3777ms ms ON s.id = ms.s_id LIMIT 4 FOR UPDATE; }
step s2li	{ SELECT s.id, s.field, ms.id AS ms FROM d3777s s JOIN d3777ms ms ON s.id = ms.s_id LIMIT 4 FOR UPDATE; }
step s2ii	{ SELECT s.id, s.field, ms.id AS ms FROM d3777s s JOIN d3777ms ms ON s.id = ms.s_id LIMIT 4 FOR UPDATE OF ms; }
step s2r	{ SELECT 's' AS t, id FROM d3777s WHERE age(xmax) = 0 UNION ALL SELECT 'ms', id FROM d3777ms WHERE age(xmax) = 0 ORDER BY 1, 2; }
step s2c	{ COMMIT; }
teardown	{ ROLLBACK; }

# the plan
permutation s2x s1c s2c
# both locked: the outer row's new version fails its qual: skipped, its lock kept; the other row returned
permutation s1f s2l s1c s2r s2c
# both locked: a new version that still passes: returned as it is now
permutation s1g s2l s1c s2r s2c
# both locked: the link row's new version fails its qual: skipped (the outer row stays locked)
permutation s1m s2l s1c s2r s2c
# both locked: the link row now points at another outer row: the merge clause fails over the two test tuples
permutation s1j s2l s1c s2r s2c
# LIMIT 1: the skipped row is not counted, so the next one is returned
permutation s1f s2one s1c s2r s2c
# the link table locked (the outer a REFERENCE mark): its new version fails its qual, then the merge clause
permutation s1m s2i s1c s2r s2c
permutation s1j s2i s1c s2r s2c
# the outer relation locked (the link table a REFERENCE mark): its new version fails its qual, then still passes
permutation s1f s2o s1c s2r s2c
permutation s1g s2o s1c s2r s2c
# Index Scans: the plan; both locked: the link row now points at another outer row (skipped, the LIMIT takes the next);
# the outer row's new version still joins (returned as it is now); the link table locked: the merge clause fails
permutation s2xi s1c s2c
permutation s1j s2li s1c s2r s2c
permutation s1f s2li s1c s2r s2c
permutation s1j s2ii s1c s2r s2c
# a new version of the link row that still joins: returned with the outer row the recheck refetched (or locked)
permutation s1n s2l s1c s2r s2c
permutation s1n s2i s1c s2r s2c
permutation s1n s2ii s1c s2r s2c

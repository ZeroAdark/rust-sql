# D3737: Django's update() / delete() filtered across a relation (`pk IN (SELECT U0.id FROM t U0 LEFT OUTER JOIN ..)`)
# over fresh tables, PostgreSQL 18.6's Hash Right Semi Join with the target hashed (or, with a filter on the LEFT-
# joined relation, a Nested Loop over a HashAggregate of the Hash Left Join and the target's index scan), against a
# concurrent change of a target row (READ COMMITTED). The write finds the version its snapshot read updated and
# rechecks the newest one under EvalPlanQual: the probe side's join reruns over its relations' refetched rows (the old
# version of the target's own second scan U0 among them), so a row whose other columns changed is written over its
# newest version, one whose key moved is skipped (U0's old version holds the old key), a deleted one is gone. The
# expected output is PostgreSQL 18.6's.
setup
{
  CREATE TABLE avatar (id serial PRIMARY KEY, "desc" text NOT NULL);
  CREATE TABLE duser (id serial PRIMARY KEY, avatar_id int REFERENCES avatar (id));
  INSERT INTO avatar ("desc") VALUES ('a'), ('b');
  INSERT INTO duser (avatar_id) VALUES (1), (NULL), (2);
  CREATE TABLE employee (id serial PRIMARY KEY, firstname text NOT NULL, lastname text NOT NULL);
  CREATE TABLE company (id serial PRIMARY KEY, name text NOT NULL, ceo_id int NOT NULL REFERENCES employee (id), point_of_contact_id int REFERENCES employee (id));
  INSERT INTO employee (firstname, lastname) VALUES ('Joe', 'Smith'), ('Frank', 'Meyer'), ('Max', 'Mustermann');
  INSERT INTO company (name, ceo_id, point_of_contact_id) VALUES ('Example Inc.', 1, 1), ('Foobar Ltd.', 2, 3), ('Test GmbH', 3, NULL);
}
teardown { DROP TABLE company, employee, duser, avatar; }

session s1
setup		{ BEGIN; }
step s1av	{ UPDATE duser SET avatar_id = 1 WHERE id = 2; }
step s1key	{ UPDATE duser SET id = 99 WHERE id = 2; }
step s1del	{ DELETE FROM duser WHERE id = 3; }
step s1ceo	{ UPDATE company SET ceo_id = 1 WHERE id = 2; }
step s1cn	{ UPDATE company SET name = 'Bar' WHERE id = 2; }
step s1cid	{ UPDATE company SET id = 98 WHERE id = 2; }
step s1c	{ COMMIT; }

session s2
step s2x	{ EXPLAIN (COSTS OFF) DELETE FROM duser WHERE duser.id IN (SELECT U0.id FROM duser U0 LEFT OUTER JOIN avatar U1 ON (U0.avatar_id = U1.id)); }
step s2dela	{ DELETE FROM duser WHERE duser.id IN (SELECT U0.id FROM duser U0 LEFT OUTER JOIN avatar U1 ON (U0.avatar_id = U1.id)) RETURNING id, avatar_id; }
step s2xf	{ EXPLAIN (COSTS OFF) DELETE FROM duser WHERE duser.id IN (SELECT U0.id FROM duser U0 LEFT OUTER JOIN avatar U1 ON (U0.avatar_id = U1.id) WHERE U1.id IS NULL OR U1."desc" = 'b'); }
step s2del	{ DELETE FROM duser WHERE duser.id IN (SELECT U0.id FROM duser U0 LEFT OUTER JOIN avatar U1 ON (U0.avatar_id = U1.id) WHERE U1.id IS NULL OR U1."desc" = 'b') RETURNING id, avatar_id; }
step s2xu	{ EXPLAIN (COSTS OFF) UPDATE company SET name = 'foo' WHERE company.id IN (SELECT U0.id FROM company U0 LEFT OUTER JOIN employee U1 ON (U0.point_of_contact_id = U1.id) INNER JOIN employee U2 ON (U0.ceo_id = U2.id) WHERE NOT (U2.firstname = (U1.firstname))); }
step s2upd	{ UPDATE company SET name = 'foo' WHERE company.id IN (SELECT U0.id FROM company U0 LEFT OUTER JOIN employee U1 ON (U0.point_of_contact_id = U1.id) INNER JOIN employee U2 ON (U0.ceo_id = U2.id) WHERE NOT (U2.firstname = (U1.firstname))) RETURNING id, name, ceo_id; }
step s2show	{ SELECT id, avatar_id FROM duser ORDER BY id; }
step s2cshow	{ SELECT id, name, ceo_id FROM company ORDER BY id; }

permutation s2x s1av s2dela s1c s2show
permutation s1key s2dela s1c s2show
permutation s2xf s1av s2del s1c s2show
permutation s1key s2del s1c s2show
permutation s1del s2del s1c s2show
permutation s2xu s1ceo s2upd s1c s2cshow
permutation s1cn s2upd s1c s2cshow
permutation s1cid s2upd s1c s2cshow

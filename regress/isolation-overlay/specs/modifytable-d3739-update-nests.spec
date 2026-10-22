# D3739: writes reaching the target through nested loops over other relations, against a concurrent change of a target
# row (READ COMMITTED): Rails' update_all across an association (the target scanned twice, its alias a nest's inner
# and the relation itself a later nest's), SQLAlchemy's UPDATE .. FROM two relations (a later nest over users) and
# Django's delete() across a relation with a pk filter (a semi nest whose inner nests itself). The write finds the
# version its snapshot read updated and rechecks the newest one under EvalPlanQual, every other relation's row
# refetched at its ctid (the target's second scan at its old version): a row whose other columns changed is written
# over its newest version, one whose join key moved or whose key left the index condition is skipped. The expected
# output is PostgreSQL 18.6's.
setup
{
  CREATE TABLE developers (id serial PRIMARY KEY, name text, salary int);
  CREATE TABLE projects (id serial PRIMARY KEY, name text);
  CREATE TABLE developers_projects (developer_id int NOT NULL, project_id int NOT NULL, joined_on date);
  INSERT INTO developers (name, salary) VALUES ('David', 80000), ('Jamis', 150000), ('Poor Jamis', 9000);
  INSERT INTO projects (name) VALUES ('Active Record'), ('Active Controller');
  INSERT INTO developers_projects VALUES (1, 1, NULL), (1, 2, NULL), (2, 1, NULL), (3, 1, NULL);
  CREATE TABLE users (id serial PRIMARY KEY, name text);
  CREATE TABLE addresses (id serial PRIMARY KEY, user_id int REFERENCES users (id), email_address text);
  CREATE TABLE dingalings (id serial PRIMARY KEY, address_id int REFERENCES addresses (id), data text);
  INSERT INTO users (name) VALUES ('jack'), ('ed'), ('fred'), ('chuck');
  INSERT INTO addresses (user_id, email_address) VALUES (1, 'jack@bean.com'), (2, 'ed@wood.com'), (2, 'ed@bettyboop.com'), (2, 'ed@lala.com'), (3, 'fred@fred.com');
  INSERT INTO dingalings (address_id, data) VALUES (2, 'ding 1/2'), (5, 'ding 2/5');
  CREATE TABLE orgunit (id serial PRIMARY KEY, name text NOT NULL);
  CREATE TABLE login (id serial PRIMARY KEY, description text NOT NULL, orgunit_id int NOT NULL REFERENCES orgunit (id));
  CREATE INDEX login_orgunit_idx ON login (orgunit_id);
  INSERT INTO orgunit (name) VALUES ('o1'), ('o2');
  INSERT INTO login (description, orgunit_id) VALUES ('l1', 1), ('l2', 2), ('l3', 1);
}
teardown { DROP TABLE login, orgunit, dingalings, addresses, users, developers_projects, projects, developers; }

session s1
setup		{ BEGIN; }
step s1sal	{ UPDATE developers SET salary = salary + 5 WHERE id = 1; }
step s1did	{ UPDATE developers SET id = 99 WHERE id = 1; }
step s1em	{ UPDATE addresses SET email_address = 'moved@x.com' WHERE id = 2; }
step s1uid	{ UPDATE addresses SET user_id = 1 WHERE id = 2; }
step s1ld	{ UPDATE login SET description = 'changed', orgunit_id = 2 WHERE id = 1; }
step s1lid	{ UPDATE login SET id = 7 WHERE id = 1; }
step s1c	{ COMMIT; }

session s2
step s2x	{ EXPLAIN (COSTS OFF) UPDATE developers __active_record_update_alias SET salary = 1 FROM developers INNER JOIN developers_projects ON developers_projects.developer_id = developers.id INNER JOIN projects ON projects.id = developers_projects.project_id WHERE projects.name = 'Active Controller' AND developers.id = __active_record_update_alias.id; }
step s2ua	{ UPDATE developers __active_record_update_alias SET salary = 1 FROM developers INNER JOIN developers_projects ON developers_projects.developer_id = developers.id INNER JOIN projects ON projects.id = developers_projects.project_id WHERE projects.name = 'Active Controller' AND developers.id = __active_record_update_alias.id RETURNING __active_record_update_alias.id, __active_record_update_alias.salary; }
step s2dshow	{ SELECT id, salary FROM developers ORDER BY id; }
step s2xa	{ EXPLAIN (COSTS OFF) UPDATE addresses SET email_address = users.name FROM users, dingalings WHERE users.id = addresses.user_id AND users.name = 'ed' AND addresses.id = dingalings.address_id AND dingalings.id = 1; }
step s2addr	{ UPDATE addresses SET email_address = users.name FROM users, dingalings WHERE users.id = addresses.user_id AND users.name = 'ed' AND addresses.id = dingalings.address_id AND dingalings.id = 1 RETURNING addresses.id, addresses.email_address; }
step s2ashow	{ SELECT id, user_id, email_address FROM addresses ORDER BY id; }
step s2xl	{ EXPLAIN (COSTS OFF) DELETE FROM login WHERE login.id IN (SELECT U0.id FROM login U0 INNER JOIN orgunit U1 ON (U0.orgunit_id = U1.id) WHERE (U0.id = 1 AND U1.name IS NOT NULL)); }
step s2ldel	{ DELETE FROM login WHERE login.id IN (SELECT U0.id FROM login U0 INNER JOIN orgunit U1 ON (U0.orgunit_id = U1.id) WHERE (U0.id = 1 AND U1.name IS NOT NULL)) RETURNING id, description; }
step s2lshow	{ SELECT id, description, orgunit_id FROM login ORDER BY id; }

permutation s2x s1sal s2ua s1c s2dshow
permutation s1did s2ua s1c s2dshow
permutation s2xa s1em s2addr s1c s2ashow
permutation s1uid s2addr s1c s2ashow
permutation s2xl s1ld s2ldel s1c s2lshow
permutation s1lid s2ldel s1c s2lshow

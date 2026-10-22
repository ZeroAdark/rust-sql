# D3611: Hibernate JOINED inheritance under a pessimistic lock (LockRows over a chain of Nested Loop Left Joins, the
# subclass tables REFERENCE-marked) against a concurrent change (READ COMMITTED). The lock waits, then rechecks the
# newest version of the party row joined to each subclass row the statement read, refetched at its ctid (the version the
# snapshot saw, a concurrent update of it not seen): a renamed party comes back renamed, its old person row with it;
# a party deleted meanwhile gives no row. The expected output is PostgreSQL 18.6's.
setup
{
  CREATE TABLE jlp_party (id int PRIMARY KEY, kind text NOT NULL, name text NOT NULL, version int NOT NULL DEFAULT 0);
  CREATE TABLE jlp_person (id int PRIMARY KEY, birth date);
  CREATE TABLE jlp_org (id int PRIMARY KEY, tax_id text);
  INSERT INTO jlp_party SELECT g, CASE WHEN g % 2 = 0 THEN 'person' ELSE 'org' END, 'p' || g FROM generate_series(1, 20000) g;
  INSERT INTO jlp_person SELECT id, date '2000-01-01' + id % 3000 FROM jlp_party WHERE kind = 'person';
  INSERT INTO jlp_org SELECT id, 't' || id FROM jlp_party WHERE kind = 'org';
  ANALYZE jlp_party;
  ANALYZE jlp_person;
  ANALYZE jlp_org;
}
teardown { DROP TABLE jlp_party, jlp_person, jlp_org; }

session s1
setup		{ BEGIN; }
step s1ren	{ UPDATE jlp_party SET name = 'renamed', version = version + 1 WHERE id = 42; }
step s1birth	{ UPDATE jlp_person SET birth = '1999-12-31' WHERE id = 42; }
step s1org	{ UPDATE jlp_party SET version = version + 1 WHERE id = 43; UPDATE jlp_org SET tax_id = 'changed' WHERE id = 43; }
step s1del	{ DELETE FROM jlp_person WHERE id = 42; DELETE FROM jlp_party WHERE id = 42; }
step s1c	{ COMMIT; }

session s2
step s2x	{ EXPLAIN (COSTS OFF) SELECT p1_0.id, p1_0.name, p1_0.version, p1_1.birth, p1_2.tax_id FROM jlp_party p1_0 LEFT JOIN jlp_person p1_1 ON p1_0.id = p1_1.id LEFT JOIN jlp_org p1_2 ON p1_0.id = p1_2.id WHERE p1_0.id = 42 FOR NO KEY UPDATE OF p1_0; }
step s2find	{ SELECT p1_0.id, p1_0.name, p1_0.version, p1_1.birth, p1_2.tax_id FROM jlp_party p1_0 LEFT JOIN jlp_person p1_1 ON p1_0.id = p1_1.id LEFT JOIN jlp_org p1_2 ON p1_0.id = p1_2.id WHERE p1_0.id = 42 FOR NO KEY UPDATE OF p1_0; }
step s2list	{ SELECT p1_0.id, p1_0.name, p1_0.version, p1_1.birth, p1_2.tax_id FROM jlp_party p1_0 LEFT JOIN jlp_person p1_1 ON p1_0.id = p1_1.id LEFT JOIN jlp_org p1_2 ON p1_0.id = p1_2.id WHERE p1_0.id IN (41, 42, 43) FOR NO KEY UPDATE OF p1_0; }

permutation s2x s1ren s2find s1c
permutation s1ren s1birth s2find s1c
permutation s1org s2list s1c
permutation s1del s2find s1c

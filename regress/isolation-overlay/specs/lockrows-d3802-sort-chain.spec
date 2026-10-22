# D3802 SORTCHAIN: TypeORM's find with an order locking a relation of a relation, LockRows -> Sort -> Nested Loop (Nested
# Loop (Nested Loop (the post's pkey scan; Hash Join (category; Hash <- the link table's bitmap scan)); the second link
# table's pkey scan); the image's pkey scan), FOR UPDATE OF the image, every other relation REFERENCE, the tables never
# analyzed. The lock reads the sorted rows. Over a newer version of an image (READ COMMITTED) PostgreSQL's EvalPlanQual
# reruns the chain over the test tuples: each REFERENCE row refetched at the row's junk ctid (the version the join read),
# the image's newest version through its index qual under the nest's parameter and its filter, and the row stays where
# the sort put it. So an image s1 changed comes back as its newest version in its old place (an order on the changed
# column is not redone), one s1 marks gone (the filter) or renumbers is skipped (a LIMIT reads on past it), a category s1
# renamed keeps the name the join read, a link s1 moved to another image keeps the row the join read, a post s1 retitled
# keeps its old title. SKIP LOCKED, NOWAIT, FOR SHARE. Expected output is PostgreSQL 18.6's.
setup
{
  CREATE TABLE scp (id serial PRIMARY KEY, title varchar NOT NULL);
  CREATE TABLE scc (id serial PRIMARY KEY, name varchar NOT NULL);
  CREATE TABLE sci (id serial PRIMARY KEY, "isRemoved" boolean NOT NULL DEFAULT false, url varchar NOT NULL);
  CREATE TABLE scpc ("postId" int NOT NULL, "categoryId" int NOT NULL, PRIMARY KEY ("postId", "categoryId"));
  CREATE TABLE scci ("categoryId" int NOT NULL, "imageId" int NOT NULL, PRIMARY KEY ("categoryId", "imageId"));
  INSERT INTO scp (title) SELECT 'p' || g FROM generate_series(1, 3) g;
  INSERT INTO scc (name) SELECT 'c' || g FROM generate_series(1, 3) g;
  INSERT INTO sci (url) SELECT 'i' || g FROM generate_series(1, 4) g;
  INSERT INTO scpc SELECT 1, g FROM generate_series(3, 1, -1) g;
  INSERT INTO scci VALUES (3, 3), (1, 4), (2, 2), (1, 1);
}
teardown { DROP TABLE scci, scpc, sci, scc, scp; }

session s1
setup		{ BEGIN; }
step s1u	{ UPDATE sci SET url = url || '!' WHERE id = 1; }
step s1z	{ UPDATE sci SET url = 'a' || url WHERE id = 3; }
step s1d	{ UPDATE sci SET url = 'gone' WHERE id = 2; }
step s1i	{ UPDATE sci SET id = 30 WHERE id = 3; }
step s1n	{ UPDATE scc SET name = 'renamed' WHERE id = 1; UPDATE sci SET url = url || '!' WHERE id = 1; }
step s1m	{ UPDATE scci SET "imageId" = 2 WHERE "categoryId" = 1 AND "imageId" = 1; UPDATE sci SET url = url || '!' WHERE id = 1; }
step s1t	{ UPDATE scp SET title = 'retitled' WHERE id = 1; UPDATE sci SET url = url || '!' WHERE id = 1; }
step s1l	{ SELECT id FROM sci WHERE id = 1 FOR UPDATE; }
step s1c	{ COMMIT; }
teardown	{ ROLLBACK; }

session s2
step s2x	{ EXPLAIN (COSTS OFF) SELECT p.id, p.title, c.name, i.id, i.url FROM scp p JOIN scpc pc ON pc."postId" = p.id JOIN scc c ON c.id = pc."categoryId" JOIN scci ci ON ci."categoryId" = c.id JOIN sci i ON i.id = ci."imageId" WHERE p.id = 1 AND i.url <> 'gone' ORDER BY c.id, i.id FOR UPDATE OF i; }
step s2l	{ SELECT p.id, p.title, c.name, i.id, i.url FROM scp p JOIN scpc pc ON pc."postId" = p.id JOIN scc c ON c.id = pc."categoryId" JOIN scci ci ON ci."categoryId" = c.id JOIN sci i ON i.id = ci."imageId" WHERE p.id = 1 AND i.url <> 'gone' ORDER BY c.id, i.id FOR UPDATE OF i; }
step s2o	{ SELECT c.id, i.id, i.url FROM scp p JOIN scpc pc ON pc."postId" = p.id JOIN scc c ON c.id = pc."categoryId" JOIN scci ci ON ci."categoryId" = c.id JOIN sci i ON i.id = ci."imageId" WHERE p.id = 1 ORDER BY i.url FOR UPDATE OF i; }
step s2lim	{ SELECT c.id, i.id, i.url FROM scp p JOIN scpc pc ON pc."postId" = p.id JOIN scc c ON c.id = pc."categoryId" JOIN scci ci ON ci."categoryId" = c.id JOIN sci i ON i.id = ci."imageId" WHERE p.id = 1 AND i.url <> 'gone' ORDER BY c.id DESC, i.id LIMIT 2 FOR UPDATE OF i; }
step s2s	{ SELECT p.id, c.name, i.id, i.url FROM scp p JOIN scpc pc ON pc."postId" = p.id JOIN scc c ON c.id = pc."categoryId" JOIN scci ci ON ci."categoryId" = c.id JOIN sci i ON i.id = ci."imageId" WHERE p.id = 1 ORDER BY c.id, i.id FOR SHARE OF i; }
step s2sk	{ SELECT p.id, c.name, i.id FROM scp p JOIN scpc pc ON pc."postId" = p.id JOIN scc c ON c.id = pc."categoryId" JOIN scci ci ON ci."categoryId" = c.id JOIN sci i ON i.id = ci."imageId" WHERE p.id = 1 ORDER BY c.id, i.id FOR UPDATE OF i SKIP LOCKED; }
step s2nw	{ SELECT p.id, c.name, i.id FROM scp p JOIN scpc pc ON pc."postId" = p.id JOIN scc c ON c.id = pc."categoryId" JOIN scci ci ON ci."categoryId" = c.id JOIN sci i ON i.id = ci."imageId" WHERE p.id = 1 ORDER BY c.id, i.id FOR UPDATE OF i NOWAIT; }

permutation s2x s1u s2l s1c
permutation s1d s2l s1c
permutation s1i s2l s1c
permutation s1n s2l s1c
permutation s1m s2l s1c
permutation s1t s2l s1c
permutation s1z s2o s1c
permutation s1d s2lim s1c
permutation s1u s2s s1c
permutation s1l s2sk s1c
permutation s1l s2nw s1c

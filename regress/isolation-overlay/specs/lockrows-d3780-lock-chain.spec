# D3780 LOCKCHAIN: TypeORM's find locking a relation of a relation, LockRows -> Nested Loop (Nested Loop (Nested Loop
# (the post's pkey scan; Hash Join (category; Hash <- the link table's bitmap scan)); the second link table's pkey
# scan); the image's pkey scan), FOR UPDATE OF the image, every other relation REFERENCE, the tables never analyzed. Over
# a newer version of an image (READ COMMITTED) PostgreSQL's EvalPlanQual reruns the joins over the test tuples: each
# REFERENCE row refetched at the row's junk ctid (the version the join read), the image's newest version through its
# index qual under the nest's parameter and its filter. So an image s1 changed comes back as its newest version, one s1
# marks gone (the filter) is skipped, a category s1 renamed keeps the name the join read, a link s1 moved to another image
# keeps the row the join read (the refetched link still names the locked image), a post s1 retitled keeps its old title.
# SKIP LOCKED, NOWAIT, FOR SHARE. Expected output is PostgreSQL 18.6's.
setup
{
  CREATE TABLE lcp (id serial PRIMARY KEY, title varchar NOT NULL);
  CREATE TABLE lcc (id serial PRIMARY KEY, name varchar NOT NULL);
  CREATE TABLE lci (id serial PRIMARY KEY, "isRemoved" boolean NOT NULL DEFAULT false, url varchar NOT NULL);
  CREATE TABLE lcpc ("postId" int NOT NULL, "categoryId" int NOT NULL, PRIMARY KEY ("postId", "categoryId"));
  CREATE TABLE lcci ("categoryId" int NOT NULL, "imageId" int NOT NULL, PRIMARY KEY ("categoryId", "imageId"));
  INSERT INTO lcp (title) SELECT 'p' || g FROM generate_series(1, 3) g;
  INSERT INTO lcc (name) SELECT 'c' || g FROM generate_series(1, 3) g;
  INSERT INTO lci (url) SELECT 'i' || g FROM generate_series(1, 3) g;
  INSERT INTO lcpc SELECT 1, g FROM generate_series(1, 3) g;
  INSERT INTO lcci SELECT g, g FROM generate_series(1, 3) g;
}
teardown { DROP TABLE lcci, lcpc, lci, lcc, lcp; }

session s1
setup		{ BEGIN; }
step s1u	{ UPDATE lci SET url = url || '!' WHERE id = 1; }
step s1d	{ UPDATE lci SET url = 'gone' WHERE id = 2; }
step s1i	{ UPDATE lci SET id = 30 WHERE id = 3; }
step s1n	{ UPDATE lcc SET name = 'renamed' WHERE id = 1; UPDATE lci SET url = url || '!' WHERE id = 1; }
step s1m	{ UPDATE lcci SET "imageId" = 2 WHERE "categoryId" = 1; UPDATE lci SET url = url || '!' WHERE id = 1; }
step s1t	{ UPDATE lcp SET title = 'retitled' WHERE id = 1; UPDATE lci SET url = url || '!' WHERE id = 1; }
step s1l	{ SELECT id FROM lci WHERE id = 1 FOR UPDATE; }
step s1c	{ COMMIT; }
teardown	{ ROLLBACK; }

session s2
step s2x	{ EXPLAIN (COSTS OFF) SELECT p.id, p.title, c.name, i.id, i.url FROM lcp p JOIN lcpc pc ON pc."postId" = p.id JOIN lcc c ON c.id = pc."categoryId" JOIN lcci ci ON ci."categoryId" = c.id JOIN lci i ON i.id = ci."imageId" WHERE p.id = 1 AND i.url <> 'gone' FOR UPDATE OF i; }
step s2l	{ SELECT p.id, p.title, c.name, i.id, i.url FROM lcp p JOIN lcpc pc ON pc."postId" = p.id JOIN lcc c ON c.id = pc."categoryId" JOIN lcci ci ON ci."categoryId" = c.id JOIN lci i ON i.id = ci."imageId" WHERE p.id = 1 AND i.url <> 'gone' FOR UPDATE OF i; }
step s2s	{ SELECT p.id, c.name, i.id, i.url FROM lcp p JOIN lcpc pc ON pc."postId" = p.id JOIN lcc c ON c.id = pc."categoryId" JOIN lcci ci ON ci."categoryId" = c.id JOIN lci i ON i.id = ci."imageId" WHERE p.id = 1 FOR SHARE OF i; }
step s2sk	{ SELECT p.id, c.name, i.id FROM lcp p JOIN lcpc pc ON pc."postId" = p.id JOIN lcc c ON c.id = pc."categoryId" JOIN lcci ci ON ci."categoryId" = c.id JOIN lci i ON i.id = ci."imageId" WHERE p.id = 1 FOR UPDATE OF i SKIP LOCKED; }
step s2nw	{ SELECT p.id, c.name, i.id FROM lcp p JOIN lcpc pc ON pc."postId" = p.id JOIN lcc c ON c.id = pc."categoryId" JOIN lcci ci ON ci."categoryId" = c.id JOIN lci i ON i.id = ci."imageId" WHERE p.id = 1 FOR UPDATE OF i NOWAIT; }

permutation s2x s1u s2l s1c
permutation s1d s2l s1c
permutation s1i s2l s1c
permutation s1n s2l s1c
permutation s1m s2l s1c
permutation s1t s2l s1c
permutation s1u s2s s1c
permutation s1l s2sk s1c
permutation s1l s2nw s1c

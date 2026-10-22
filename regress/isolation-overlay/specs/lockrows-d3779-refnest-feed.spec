# D3779 REFNEST-FEED: TypeORM's paginated find with a lock, the page's distinct ids over the locking query: Limit ->
# Unique -> Sort -> Subquery Scan -> LockRows -> Nested Loop Left Join (the post's pkey scan, FOR UPDATE OF the post;
# the user's pkey scan, REFERENCE), the tables never analyzed. Over a newer version of the post (READ COMMITTED)
# PostgreSQL's EvalPlanQual reruns the join over the test tuple: the post's newest version through its index quals, the
# nest's parameter from it, the user refetched at the row's junk ctid (the version the join read; none for a
# null-extended row) and rechecked under that parameter, then the join qual; LockRows hands the row on through the
# Subquery Scan into the Sort. So a post moved to another author comes back null-extended (the refetched old user fails
# the new parameter), a post whose author s1 cleared comes back null-extended, a renamed user keeps the name the join
# read, a post whose id s1 changed is skipped (its newest version fails the index qual). SKIP LOCKED, NOWAIT, FOR SHARE.
# s2h: the range form (PostgreSQL may hash the users: Hash Left Join under the LockRows, REFFETCH's feeding form).
# Expected output is PostgreSQL 18.6's.
setup
{
  CREATE TABLE fu (id serial PRIMARY KEY, name varchar NOT NULL);
  CREATE TABLE fp (id serial PRIMARY KEY, title varchar NOT NULL, "authorId" int REFERENCES fu (id));
  INSERT INTO fu (name) VALUES ('Timber'), ('Umed'), ('Dima');
  INSERT INTO fp (title, "authorId") VALUES ('About Timber', 1), ('About Umed', 2), ('Anonymous', NULL), ('More Timber', 1);
}
teardown { DROP TABLE fp, fu; }

session s1
setup		{ BEGIN; }
step s1a	{ UPDATE fp SET title = title || '!' WHERE id = 1; }
step s1m	{ UPDATE fp SET "authorId" = 2 WHERE id = 1; }
step s1z	{ UPDATE fp SET "authorId" = NULL WHERE id = 1; }
step s1g	{ UPDATE fp SET "authorId" = 3 WHERE id = 3; }
step s1i	{ UPDATE fp SET id = 10 WHERE id = 1; }
step s1r	{ UPDATE fu SET name = 'renamed' WHERE id = 1; UPDATE fp SET title = title || '!' WHERE id = 1; }
step s1l	{ SELECT id FROM fp WHERE id = 1 FOR UPDATE; }
step s1c	{ COMMIT; }
teardown	{ ROLLBACK; }

session s2
step s2x	{ EXPLAIN (COSTS OFF) SELECT DISTINCT "distinctAlias"."Post_id" AS "ids_Post_id" FROM (SELECT "Post"."id" AS "Post_id", "Post"."title" AS "Post_title", "Post"."authorId" AS "Post_authorId", "Post__Post_author"."id" AS "Post__Post_author_id", "Post__Post_author"."name" AS "Post__Post_author_name" FROM fp "Post" LEFT JOIN fu "Post__Post_author" ON "Post__Post_author"."id"="Post"."authorId" WHERE (("Post"."id" = 1)) FOR UPDATE OF "Post") "distinctAlias" ORDER BY "Post_id" ASC LIMIT 1; }
step s2l	{ SELECT DISTINCT "distinctAlias"."Post_id" AS "ids_Post_id" FROM (SELECT "Post"."id" AS "Post_id", "Post"."title" AS "Post_title", "Post"."authorId" AS "Post_authorId", "Post__Post_author"."id" AS "Post__Post_author_id", "Post__Post_author"."name" AS "Post__Post_author_name" FROM fp "Post" LEFT JOIN fu "Post__Post_author" ON "Post__Post_author"."id"="Post"."authorId" WHERE (("Post"."id" = 1)) FOR UPDATE OF "Post") "distinctAlias" ORDER BY "Post_id" ASC LIMIT 1; }
step s2n	{ SELECT DISTINCT d.pid, d.title, d.uname FROM (SELECT p.id AS pid, p.title, u.id AS uid, u.name AS uname FROM fp p LEFT JOIN fu u ON u.id = p."authorId" WHERE p.id IN (1, 3, 4) FOR UPDATE OF p) d ORDER BY d.pid LIMIT 5; }
step s2s	{ SELECT DISTINCT d.pid, d.uname FROM (SELECT p.id AS pid, u.name AS uname FROM fp p LEFT JOIN fu u ON u.id = p."authorId" WHERE p.id IN (1, 3, 4) FOR SHARE OF p) d ORDER BY d.pid LIMIT 5; }
step s2sk	{ SELECT DISTINCT d.pid, d.uname FROM (SELECT p.id AS pid, u.name AS uname FROM fp p LEFT JOIN fu u ON u.id = p."authorId" WHERE p.id IN (1, 3, 4) FOR UPDATE OF p SKIP LOCKED) d ORDER BY d.pid LIMIT 5; }
step s2nw	{ SELECT DISTINCT d.pid FROM (SELECT p.id AS pid, u.name AS uname FROM fp p LEFT JOIN fu u ON u.id = p."authorId" WHERE p.id = 1 FOR UPDATE OF p NOWAIT) d ORDER BY d.pid LIMIT 1; }
step s2c	{ SELECT count(*), count(d.uname), max(d.title) FROM (SELECT p.id AS pid, p.title, u.name AS uname FROM fp p LEFT JOIN fu u ON u.id = p."authorId" WHERE p.id IN (1, 3) FOR UPDATE OF p) d; }
step s2hx	{ EXPLAIN (COSTS OFF) SELECT DISTINCT d.pid, d.uname FROM (SELECT p.id AS pid, u.name AS uname FROM fp p LEFT JOIN fu u ON u.id = p."authorId" WHERE p.id <= 4 FOR UPDATE OF p) d ORDER BY d.pid; }
step s2h	{ SELECT DISTINCT d.pid, d.uname FROM (SELECT p.id AS pid, u.name AS uname FROM fp p LEFT JOIN fu u ON u.id = p."authorId" WHERE p.id <= 4 FOR UPDATE OF p) d ORDER BY d.pid; }

permutation s2x s1a s2l s1c
permutation s1i s2l s1c
permutation s1a s2n s1c
permutation s1m s2n s1c
permutation s1z s2n s1c
permutation s1g s2n s1c
permutation s1r s2n s1c
permutation s1i s2n s1c
permutation s1m s2s s1c
permutation s1l s2sk s1c
permutation s1l s2nw s1c
permutation s1z s2c s1c
permutation s2hx s1m s2h s1c
permutation s1z s2h s1c
permutation s1r s2h s1c
permutation s1i s2h s1c

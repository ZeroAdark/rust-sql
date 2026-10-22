# D3605: Rails' delete_all / update_all over a two-level association (`DELETE FROM comments WHERE comments.id IN (SELECT
# comments.id FROM comments JOIN posts .. JOIN authors .. WHERE authors.banned)`) planned as a Hash Semi Join whose
# Hash holds Nested Loops over the posts and comments indexes (enable_hashagg and enable_sort off), against a concurrent
# change of a listed comment (READ COMMITTED). The write finds the version its snapshot read updated and rechecks the
# newest one under EvalPlanQual, which refetches the subquery's REFERENCE rows as the join read them and reruns the
# build: a comment edited is deleted over its newest version, one moved to another post still is (its subquery row is
# the version read), one deleted is gone, one whose id changed no longer joins and is skipped. The expected output is
# PostgreSQL 18.6's.
setup
{
  CREATE TABLE authors (id bigserial PRIMARY KEY, banned boolean NOT NULL DEFAULT false, name text);
  CREATE TABLE posts (id bigserial PRIMARY KEY, author_id bigint NOT NULL REFERENCES authors(id), title text);
  CREATE INDEX index_posts_on_author_id ON posts (author_id);
  CREATE TABLE comments (id bigserial PRIMARY KEY, post_id bigint NOT NULL REFERENCES posts(id), body text, flagged boolean NOT NULL DEFAULT false);
  CREATE INDEX index_comments_on_post_id ON comments (post_id);
  INSERT INTO authors (banned, name) SELECT g = 250, 'a' || g FROM generate_series(1, 5000) g;
  INSERT INTO posts (author_id, title) SELECT g % 5000 + 1, 't' || g FROM generate_series(1, 30000) g;
  INSERT INTO comments (post_id, body) SELECT g % 30000 + 1, 'b' || g FROM generate_series(1, 90000) g;
  ANALYZE authors; ANALYZE posts; ANALYZE comments;
}
teardown { DROP TABLE comments, posts, authors; }

session s1
setup		{ BEGIN; }
step s1body	{ UPDATE comments SET body = 'edited' WHERE id = 248; }
step s1move	{ UPDATE comments SET post_id = 1 WHERE id = 248; }
step s1del	{ DELETE FROM comments WHERE id = 248; }
step s1id	{ UPDATE comments SET id = 900248 WHERE id = 248; }
step s1c	{ COMMIT; }

session s2
setup		{ SET enable_hashagg = off; SET enable_sort = off; }
step s2x	{ EXPLAIN (COSTS OFF) DELETE FROM comments WHERE comments.id IN (SELECT comments.id FROM comments INNER JOIN posts ON posts.id = comments.post_id INNER JOIN authors ON authors.id = posts.author_id WHERE authors.banned = TRUE); }
step s2del	{ DELETE FROM comments WHERE comments.id IN (SELECT comments.id FROM comments INNER JOIN posts ON posts.id = comments.post_id INNER JOIN authors ON authors.id = posts.author_id WHERE authors.banned = TRUE); }
step s2upd	{ UPDATE comments SET flagged = TRUE WHERE comments.id IN (SELECT comments.id FROM comments INNER JOIN posts ON posts.id = comments.post_id INNER JOIN authors ON authors.id = posts.author_id WHERE authors.banned = TRUE) RETURNING id, post_id, body; }
step s2show	{ SELECT id, post_id, body, flagged FROM comments WHERE id IN (248, 900248, 30248) ORDER BY id; }
step s2count	{ SELECT count(*), count(*) FILTER (WHERE flagged) FROM comments; }

permutation s2x s1body s2del s1c s2show s2count
permutation s1move s2del s1c s2show s2count
permutation s1del s2del s1c s2show s2count
permutation s1id s2del s1c s2show s2count
permutation s1body s2upd s1c s2show s2count
permutation s1move s2upd s1c s2show s2count
permutation s1id s2upd s1c s2show s2count

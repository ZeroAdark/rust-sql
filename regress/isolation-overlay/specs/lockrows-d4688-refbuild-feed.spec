# D4688 REFBUILD-FEED: a row lock in a subquery over an INNER hash join whose build is the locked relation t, the lock's
# rows feeding an aggregate (READ COMMITTED). The lock waits on a row another session changed and rechecks the newest
# version joined to the REFERENCE row the join read (a key moved off the join is dropped, a non-key change is counted
# with its new value, a deleted row is gone); a change that does not conflict with the lock strength never waits; a
# change of the REFERENCE relation j is never waited on (j is not locked) and the join reads the version it saw.
# Each prints the aggregate. The expected output is PostgreSQL 18.6's.
setup
{
  CREATE TABLE d4688i_t (id int PRIMARY KEY, v int NOT NULL);
  INSERT INTO d4688i_t SELECT g, g * 10 FROM generate_series(1, 1000) g;
  CREATE TABLE d4688i_j (id int PRIMARY KEY, w int NOT NULL);
  INSERT INTO d4688i_j SELECT g, g % 7 FROM generate_series(1, 1000, 2) g;
  ANALYZE d4688i_t, d4688i_j;
}
teardown { DROP TABLE d4688i_t, d4688i_j; }

session s1
step s1key	{ BEGIN; UPDATE d4688i_t SET id = 1005 WHERE id = 5; }
step s1del	{ BEGIN; DELETE FROM d4688i_t WHERE id = 15; }
step s1nk	{ BEGIN; UPDATE d4688i_t SET v = -1 WHERE id = 25; }
step s1j	{ BEGIN; UPDATE d4688i_j SET w = 100 WHERE id = 35; }
step s1com	{ COMMIT; }

session s2
step s2pl	{ EXPLAIN (COSTS OFF) SELECT count(*), sum(v) FROM (SELECT t.id, t.v FROM d4688i_t t JOIN d4688i_j j USING (id) WHERE t.id % 5 = 0 FOR NO KEY UPDATE OF t) l; }
step s2ks	{ SELECT count(*), md5(string_agg(id::text, ',' ORDER BY id)) FROM (SELECT t.id FROM d4688i_t t JOIN d4688i_j j USING (id) WHERE t.id % 5 = 0 FOR KEY SHARE OF t) l; }
step s2nk	{ SELECT count(*), sum(v) FROM (SELECT t.id, t.v FROM d4688i_t t JOIN d4688i_j j USING (id) WHERE t.id % 5 = 0 FOR NO KEY UPDATE OF t) l; }
step s2w	{ SELECT w, count(*) FROM (SELECT t.id, j.w FROM d4688i_t t JOIN d4688i_j j USING (id) WHERE t.id % 5 = 0 FOR UPDATE OF t) l GROUP BY w ORDER BY w; }

permutation s2pl
permutation s1key s2ks s1com
permutation s1del s2ks s1com
permutation s1nk s2ks s1com
permutation s1nk s2nk s1com
permutation s1key s2nk s1com
permutation s1j s2w s1com

# D2747 SEMINEST (modifytable): an UPDATE / DELETE over a Nested Loop Semi Join (EXISTS over a heap scan of a small
# relation, the target's own scan the outer) meets a target row s1 changed. PostgreSQL's EvalPlanQual re-runs the join
# over the target's newest version and the inner row the statement joined, refetched by its junk ctid (the version it
# read, even when s1 changed or deleted that inner row since): a row that still joins it and still meets the target's
# quals is written; one s1 moved out of the target's qual, moved off the join key, or deleted is skipped.

setup
{
 CREATE TABLE sem_t (id int PRIMARY KEY, ref int, tier int, note text);
 INSERT INTO sem_t SELECT i, i, i % 3, 't' || i FROM generate_series(1, 400) i;
 CREATE TABLE sem_s (id int PRIMARY KEY, k int);
 INSERT INTO sem_s SELECT i, 380 + i FROM generate_series(1, 20) i;
 ANALYZE sem_t, sem_s;
}

teardown
{
 DROP TABLE sem_s;
 DROP TABLE sem_t;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1u	{ UPDATE sem_t SET note = note || '*' WHERE id IN (391, 392); }
step s1q	{ UPDATE sem_t SET tier = 9 WHERE id = 393; }
step s1d	{ DELETE FROM sem_t WHERE id = 394; }
step s1m	{ UPDATE sem_t SET ref = ref + 1000 WHERE id = 395; }
step s1s	{ UPDATE sem_s SET k = 0 WHERE k = 396; UPDATE sem_t SET note = note || '+' WHERE id = 396; }
step s1c	{ COMMIT; }

session s2
setup		{ SET enable_hashjoin = off; SET enable_mergejoin = off; SET enable_material = off; BEGIN ISOLATION LEVEL READ COMMITTED; }
step s2d	{ DELETE FROM sem_t t WHERE EXISTS (SELECT 1 FROM sem_s s WHERE s.k = t.ref) AND t.id > 388 AND t.tier < 5; }
step s2u	{ UPDATE sem_t t SET note = note || '#' WHERE EXISTS (SELECT 1 FROM sem_s s WHERE s.k = t.ref) AND t.id > 388 AND t.tier < 5; }
step s2c	{ COMMIT; }

session s3
setup		{ SET enable_hashjoin = off; SET enable_mergejoin = off; SET enable_material = off; }
step s3s	{ SELECT id, ref, tier, note FROM sem_t WHERE id > 388 ORDER BY id; }
step s3e	{ EXPLAIN (COSTS OFF) DELETE FROM sem_t t WHERE EXISTS (SELECT 1 FROM sem_s s WHERE s.k = t.ref) AND t.id > 388 AND t.tier < 5; }

permutation s3e s1u s1q s1d s1m s1s s2d s1c s2c s3s
permutation s1u s1q s1d s1m s1s s2u s1c s2c s3s

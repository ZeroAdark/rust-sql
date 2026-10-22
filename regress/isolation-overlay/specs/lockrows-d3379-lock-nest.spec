# D3379 LOCKNEST: Limit -> LockRows -> Nested Loop, FOR .. OF the nest's inner relation (b, a parameterized index scan,
# a Memoize over it or not), the outer a REFERENCE-marked relation's index scan. A concurrent update of a locked b row:
# EvalPlanQual reruns the join over the outer row the join read (a's REFERENCE row, refetched at its ctid: the version
# read, even when a changed too) and b's newest version, its index condition and filter rechecked under the nest's
# parameters, then the join qual; LockRows hands the row to the Limit, so a row the recheck drops makes the Limit read
# one more. The forms: a kept row (its newest version returned), a key moved off the join (dropped), a filter failed,
# a deleted row, OFFSET, SKIP LOCKED, both relations changed, and the same without the Memoize. Expected output is
# PostgreSQL 18.6's.

setup
{
 CREATE TABLE d79a (id int PRIMARY KEY, bid int, k int);
 CREATE TABLE d79b (id int PRIMARY KEY, w int, name text);
 INSERT INTO d79a SELECT i, i % 4, i FROM generate_series(1, 12) i;
 INSERT INTO d79b SELECT i, i % 2, 'b' || i FROM generate_series(0, 3) i;
 ANALYZE d79a, d79b;
}

teardown
{
 DROP TABLE d79a, d79b;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1n	{ UPDATE d79b SET name = name || '+' WHERE id = 1; }
step s1k	{ UPDATE d79b SET id = 10 WHERE id = 1; }
step s1f	{ UPDATE d79b SET name = 'x' WHERE id = 1; }
step s1d	{ DELETE FROM d79b WHERE id = 1; }
step s1a	{ UPDATE d79a SET bid = 2 WHERE id = 1; UPDATE d79b SET name = 'new1' WHERE id = 1; }
step s1c	{ COMMIT; }

session s2
setup
{
 SET enable_hashjoin = off;
 SET enable_mergejoin = off;
 SET enable_material = off;
 SET enable_seqscan = off;
}
step s2x	{ EXPLAIN (COSTS OFF) SELECT a.id, b.id AS b, b.name FROM d79a a JOIN d79b b ON b.id = a.bid WHERE a.k <= 9 AND b.name <> 'x' LIMIT 3 FOR UPDATE OF b; }
step s2l	{ SELECT a.id, b.id AS b, b.name FROM d79a a JOIN d79b b ON b.id = a.bid WHERE a.k <= 9 AND b.name <> 'x' LIMIT 3 FOR UPDATE OF b; }
step s2o	{ SELECT a.id, b.id AS b, b.name FROM d79a a JOIN d79b b ON b.id = a.bid WHERE a.k <= 9 AND b.name <> 'x' LIMIT 2 OFFSET 1 FOR NO KEY UPDATE OF b; }
step s2s	{ SELECT a.id, b.id AS b, b.name FROM d79a a JOIN d79b b ON b.id = a.bid WHERE a.k <= 9 LIMIT 3 FOR SHARE OF b SKIP LOCKED; }
step s2m	{ SET enable_memoize = off; }

permutation s2x s1n s2l s1c
permutation s1k s2l s1c
permutation s1f s2l s1c
permutation s1d s2l s1c
permutation s1k s2o s1c
permutation s1n s2s s1c
permutation s1a s2l s1c
permutation s2m s2x s1k s2l s1c
permutation s2m s1a s2l s1c

# D3379 LOCKPAIR: Limit -> LockRows -> Nested Loop with both relations locked (FOR UPDATE with no OF list, or one clause
# for each): the outer a's own scan (a seq scan here) and the inner b's parameterized index scan (a Memoize over it or
# not). ExecLockRows locks a's row, then b's; when either lock traversed to a newer version EvalPlanQual reruns the join
# over both test tuples (each mark's newest version where its lock traversed, else the version it locked): a's filter
# over a's, the nest's parameter from it, b's index condition and filter over b's under that parameter, then LockRows
# hands the row to the Limit, which reads one more row for each the recheck drops. The forms: b kept (newest), b's key
# moved off the join, a's filter failed, a's join key moved (the old b no longer matches the new parameter), both
# changed, a deleted, SKIP LOCKED on b (a's lock kept), mixed strengths, OFFSET, and the same without the Memoize.
# D3379 PAIRPROBE (s2w): b's filter b.w = 1 costs the parameterized path out, so the inner is b's unparameterized scan
# under the join filter, which pgrust reads as a catalog lookup (b hashed once, a probing it): the recheck redoes a's
# filter over a's test tuple, then b's filter over b's, then the join qual; b's w moved off the filter drops each row
# joined to it, b's new name is returned, a's filter failed drops a's row.
# Expected output is PostgreSQL 18.6's.

setup
{
 CREATE TABLE d80a (id int PRIMARY KEY, bid int, k int);
 CREATE TABLE d80b (id int PRIMARY KEY, w int, name text);
 INSERT INTO d80a SELECT i, i % 4, i FROM generate_series(1, 12) i;
 INSERT INTO d80b SELECT i, i % 2, 'b' || i FROM generate_series(0, 3) i;
 ANALYZE d80a, d80b;
}

teardown
{
 DROP TABLE d80a, d80b;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1n	{ UPDATE d80b SET name = name || '+' WHERE id = 1; }
step s1k	{ UPDATE d80b SET id = 10 WHERE id = 1; }
step s1f	{ UPDATE d80a SET k = 100 WHERE id = 1; }
step s1j	{ UPDATE d80a SET bid = 2 WHERE id = 1; }
step s1b	{ UPDATE d80a SET k = k WHERE id = 1; UPDATE d80b SET name = 'both' WHERE id = 1; }
step s1d	{ DELETE FROM d80a WHERE id = 1; }
step s1h	{ UPDATE d80b SET w = w WHERE id = 2; }
step s1w	{ UPDATE d80b SET w = 0 WHERE id = 1; }
step s1m	{ UPDATE d80b SET name = name || '+' WHERE id = 3; }
step s1c	{ COMMIT; }

session s2
setup
{
 SET enable_hashjoin = off;
 SET enable_mergejoin = off;
 SET enable_material = off;
 SET enable_seqscan = off;
}
step s2x	{ EXPLAIN (COSTS OFF) SELECT a.id, a.k, b.id AS b, b.name FROM d80a a JOIN d80b b ON b.id = a.bid WHERE a.k <= 9 AND b.name <> 'x' LIMIT 3 FOR UPDATE; }
step s2l	{ SELECT a.id, a.k, b.id AS b, b.name FROM d80a a JOIN d80b b ON b.id = a.bid WHERE a.k <= 9 AND b.name <> 'x' LIMIT 3 FOR UPDATE; }
step s2m	{ SELECT a.id, a.k, b.id AS b, b.name FROM d80a a JOIN d80b b ON b.id = a.bid WHERE a.k <= 9 LIMIT 3 FOR NO KEY UPDATE OF a FOR SHARE OF b; }
step s2o	{ SELECT a.id, b.id AS b, b.name FROM d80a a JOIN d80b b ON b.id = a.bid WHERE a.k <= 9 LIMIT 2 OFFSET 1 FOR UPDATE; }
step s2s	{ BEGIN; SELECT a.id, b.id AS b, b.name FROM d80a a JOIN d80b b ON b.id = a.bid WHERE a.k <= 9 LIMIT 3 FOR UPDATE SKIP LOCKED; }
step s2wx	{ EXPLAIN (COSTS OFF) SELECT a.id, b.id AS b, b.name FROM d80a a JOIN d80b b ON b.id = a.bid WHERE b.w = 1 AND a.k % 2 = 1 LIMIT 3 FOR UPDATE; }
step s2w	{ SELECT a.id, b.id AS b, b.name FROM d80a a JOIN d80b b ON b.id = a.bid WHERE b.w = 1 AND a.k % 2 = 1 LIMIT 3 FOR UPDATE; }
step s2c	{ COMMIT; }
step s2z	{ SET enable_memoize = off; }

session s3
step s3a	{ SELECT id FROM d80a WHERE id = 2 FOR UPDATE NOWAIT; }

permutation s2x s1n s2l s1c
permutation s1k s2l s1c
permutation s1f s2l s1c
permutation s1j s2l s1c
permutation s1b s2l s1c
permutation s1d s2l s1c
permutation s1n s2m s1c
permutation s1k s2o s1c
permutation s1h s2s s3a s2c s1c
permutation s2z s2x s1j s2l s1c
permutation s2z s1b s2l s1c
permutation s2wx s1w s2w s1c
permutation s1m s2w s1c
permutation s1f s2w s1c

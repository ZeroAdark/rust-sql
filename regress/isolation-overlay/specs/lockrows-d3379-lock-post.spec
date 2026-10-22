# lockrows D3379 LOCKPOST: a row lock beside a data-modifying CTE nothing reads and without RETURNING, whose rows run
# once the statement's plan has ended (ExecPostprocessPlan). s1 changes a row s2's lock waits for (READ COMMITTED): the
# lock's recheck runs over the newest version (a qual over it checked again, a deleted row gone, a LIMIT counting the
# rechecked row), then the CTE writes, its own rechecks meeting s1's committed versions and s2's own locks.
# Expected output is the C oracle's (PostgreSQL 18, pgdg).
setup
{
  CREATE TABLE lp (k int PRIMARY KEY, v text NOT NULL, n int NOT NULL);
  INSERT INTO lp SELECT i, 'v' || i, i % 3 FROM generate_series(1, 10) i;
}

teardown
{
  DROP TABLE lp;
}

session s1
setup		{ BEGIN; }
step s1u	{ UPDATE lp SET v = 's1', n = n + 10 WHERE k = 2; }
step s1d	{ DELETE FROM lp WHERE k = 3; }
step s1c	{ COMMIT; }
step s1a	{ ROLLBACK; }
teardown	{ ROLLBACK; }

session s2
step s2l	{ WITH u AS (UPDATE lp SET v = 'cte' WHERE k IN (2, 4)) SELECT k, v, n FROM lp WHERE k <= 4 ORDER BY k FOR UPDATE; }
step s2w	{ WITH u AS (UPDATE lp SET v = 'cte' WHERE k = 4) SELECT k, v, n FROM lp WHERE k <= 4 AND n < 5 ORDER BY k FOR UPDATE; }
step s2m	{ WITH u AS (DELETE FROM lp WHERE k = 1) SELECT k, v FROM lp WHERE k <= 4 ORDER BY k LIMIT 2 FOR UPDATE; }
step s2s	{ SELECT k, v, n FROM lp WHERE k <= 5 ORDER BY k; }

# the recheck over s1's version of row 2, then the CTE writes rows 2 and 4
permutation s1u s2l s1c s2s
# the qual checked again over the newest version: n = 12 fails it, row 2 is not returned (the CTE writes row 4)
permutation s1u s2w s1c s2s
# a deleted row gives none
permutation s1d s2l s1c s2s
# under a LIMIT the rechecked row counts; the CTE deletes row 1
permutation s1u s2m s1c s2s
# s1 rolls back: the lock takes the row as read
permutation s1u s2l s1a s2s

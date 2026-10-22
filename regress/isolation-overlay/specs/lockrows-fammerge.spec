# lockrows FAMMERGE (gen part-0048): a LIMIT / OFFSET above a row lock over a MergeAppend of a partitioned table's leaves
# (ORDER BY at LIMIT .. FOR UPDATE, each leaf an index scan in the merge keys' order). ExecLockRows locks each row in the
# leaf its tableoid names; over a newer version its EvalPlanQual reruns the merge over that leaf's test tuple (the other
# leaves' marks inactive: their scans empty) and hands the row to the Limit, which counts only the rows the lock returns.
# The second leaf is attached in another column order with a dropped column. Forms: a newer version that still passes,
# one moved out of the quals (the next merged row taken), OFFSET, SKIP LOCKED past a row another session holds, NOWAIT.
# Expected output is PostgreSQL 18.6's.
setup
{
  CREATE TABLE pe (k int, acct int, at int, v text) PARTITION BY RANGE (k);
  CREATE TABLE pe1 PARTITION OF pe FOR VALUES FROM (0) TO (100);
  CREATE TABLE pe2 (v text, gone int, at int, acct int, k int);
  ALTER TABLE pe2 DROP COLUMN gone;
  ALTER TABLE pe ATTACH PARTITION pe2 FOR VALUES FROM (100) TO (200);
  CREATE TABLE pe3 PARTITION OF pe FOR VALUES FROM (200) TO (300);
  CREATE INDEX pe_acct_at ON pe (acct, at);
  INSERT INTO pe SELECT g, g % 10, (g * 7) % 300, 'x' FROM generate_series(0, 299) g;
  ANALYZE pe;
}
teardown { DROP TABLE pe; }

session s1
setup		{ BEGIN; }
step s1u	{ UPDATE pe SET v = 'y' WHERE k = 45; }
step s1m	{ UPDATE pe SET acct = 6 WHERE k = 45; }
step s1l	{ SELECT k FROM pe WHERE k = 215 FOR UPDATE; }
step s1c	{ COMMIT; }
teardown	{ ROLLBACK; }

session s2
setup		{ SET enable_seqscan = off; SET enable_bitmapscan = off; SET enable_sort = off; }
step s2x	{ EXPLAIN (COSTS OFF) SELECT tableoid::regclass, k, at, v FROM pe WHERE acct = 5 ORDER BY at LIMIT 3 FOR UPDATE; }
step s2l	{ SELECT tableoid::regclass, k, at, v FROM pe WHERE acct = 5 ORDER BY at LIMIT 3 FOR UPDATE; }
step s2o	{ SELECT tableoid::regclass, k, at, v FROM pe WHERE acct = 5 ORDER BY at LIMIT 2 OFFSET 1 FOR UPDATE; }
step s2s	{ SELECT tableoid::regclass, k, at, v FROM pe WHERE acct = 5 ORDER BY at LIMIT 3 FOR UPDATE SKIP LOCKED; }
step s2n	{ SELECT tableoid::regclass, k, at, v FROM pe WHERE acct = 5 ORDER BY at LIMIT 3 FOR UPDATE NOWAIT; }

# acct 5 by at: k 215 (pe3, at 5), k 45 (pe1, 15), k 175 (pe2, 25), k 5 (pe1, 35), ..
permutation s2x s1u s2l s1c
permutation s1m s2l s1c
permutation s1m s2o s1c
permutation s1l s2s s1c
permutation s1l s2n s1c

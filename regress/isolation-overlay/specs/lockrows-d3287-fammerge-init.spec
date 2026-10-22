# lockrows D3287 FAMMERGE-INIT: a LIMIT above a row lock over a MergeAppend of a partitioned family's leaf index scans
# (Limit -> LockRows -> Merge Append -> each leaf's Index Scan), the InitPlan an index key (`acct = (SELECT ..)`): the
# job queue's ordered pop beside a config row. When s1 changed a row (READ COMMITTED), EvalPlanQual reruns the Merge
# Append over the newest version in the leaf the row's tableoid names (the other leaves' marks inactive), the InitPlan's
# value copied from the main run (EvalPlanQualStart):
#  s1v: row 1027's v changed: returned with its new value.
#  s1a: row 1027 moved to another acct: its newest version fails the index key, skipped, the next row taken.
#  s1m: row 1027 moved to another partition: 40001 (the tuple to be locked was already moved to another partition).
#  s1q s1v: the config row s1 changed does not move the copied value.
#  s1l: s1 holds row 1027: SKIP LOCKED takes the next row.
# Rows of acct 7 by at: 1567 (8, mj1), 1027 (18, mj1), 487 (28, mj2), 1947 (75, mj2), 1407 (85, mjd), ...
# Expected output is PostgreSQL 18.6's (the C oracle).
setup
{
  CREATE TABLE mj (id int NOT NULL, acct int NOT NULL, at int NOT NULL, v text NOT NULL) PARTITION BY RANGE (at);
  CREATE TABLE mj1 PARTITION OF mj FOR VALUES FROM (0) TO (20);
  CREATE TABLE mj2 PARTITION OF mj FOR VALUES FROM (20) TO (80);
  CREATE TABLE mjd PARTITION OF mj DEFAULT;
  CREATE INDEX mj_acct_at ON mj (acct, at);
  INSERT INTO mj SELECT g, g % 20, (g * 37) % 1999, 'v' || g FROM generate_series(1, 2000) g;
  CREATE TABLE mcfg (name text PRIMARY KEY, b int NOT NULL);
  INSERT INTO mcfg VALUES ('s', 7);
  ANALYZE mj, mcfg;
}
teardown { DROP TABLE mj, mcfg; }

session s1
setup		{ BEGIN; }
step s1v	{ UPDATE mj SET v = v || '+' WHERE id = 1027; }
step s1a	{ UPDATE mj SET acct = 8 WHERE id = 1027; }
step s1m	{ UPDATE mj SET at = 150 WHERE id = 1027; }
step s1q	{ UPDATE mcfg SET b = 8 WHERE name = 's'; }
step s1l	{ SELECT id FROM mj WHERE id = 1027 FOR UPDATE; }
step s1c	{ COMMIT; }
teardown	{ ROLLBACK; }

session s2
setup		{ SET enable_sort = off; SET enable_seqscan = off; SET enable_bitmapscan = off; }
step s2x	{ EXPLAIN (COSTS OFF) SELECT id, at, v FROM mj WHERE acct = (SELECT b FROM mcfg WHERE name = 's') ORDER BY at LIMIT 3 FOR UPDATE; }
step s2l	{ SELECT tableoid::regclass, id, at, v FROM mj WHERE acct = (SELECT b FROM mcfg WHERE name = 's') ORDER BY at LIMIT 3 FOR UPDATE; }
step s2sk	{ SELECT tableoid::regclass, id, at, v FROM mj WHERE acct = (SELECT b FROM mcfg WHERE name = 's') ORDER BY at LIMIT 3 FOR UPDATE SKIP LOCKED; }

permutation s2x s1c
permutation s1v s2l s1c
permutation s1a s2l s1c
permutation s1m s2l s1c
permutation s1q s1v s2l s1c
permutation s1l s2sk s1c

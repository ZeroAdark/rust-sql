# lockrows (D652-S1's lock half): a row lock over its own index scan beside a correlated SubPlan over an aggregate inner
# (its rerun set the only other pipelines). When s1 changed the row (READ COMMITTED), EvalPlanQual reruns the plan below
# LockRows over the newest version: the SubPlan runs again with the new row's arguments, under the statement's snapshot
# (s1's committed ll row stays unseen), a qual over it is checked again, and a LIMIT above counts the rechecked row (or
# the next one when it fails). The recheck runs in the IR (ops/lockrows/lower.rs subs_in_ir).
# Expected output is the C oracle's (PostgreSQL 18, pgdg).
setup
{
  CREATE TABLE la (id int PRIMARY KEY, grp int NOT NULL, v int NOT NULL);
  INSERT INTO la SELECT i, i % 4, i FROM generate_series(1, 40) i;
  CREATE TABLE ll (id int PRIMARY KEY, acct int NOT NULL, amt int NOT NULL);
  INSERT INTO ll SELECT i, i % 4, i * 3 % 50 FROM generate_series(1, 200) i;
  CREATE INDEX ll_acct ON ll (acct);
  ANALYZE la, ll;
}

teardown
{
  DROP TABLE la, ll;
}

session s1
setup		{ BEGIN; }
step s1g	{ UPDATE la SET grp = 3 WHERE id = 5; }
step s1i	{ INSERT INTO ll VALUES (1000, 3, 99); }
step s1v	{ UPDATE la SET v = 100 WHERE id = 5; }
step s1c	{ COMMIT; }
teardown	{ ROLLBACK; }

session s2
setup		{ SET enable_seqscan = off; SET enable_bitmapscan = off; }
step s2x	{ EXPLAIN (COSTS OFF) SELECT a.id, a.grp, (SELECT max(l.amt) FROM ll l WHERE l.acct = a.grp) AS m FROM la a WHERE a.id <= 8 ORDER BY a.id FOR UPDATE; }
step s2t	{ SELECT a.id, a.grp, (SELECT max(l.amt) FROM ll l WHERE l.acct = a.grp) AS m FROM la a WHERE a.id <= 8 ORDER BY a.id FOR UPDATE; }
step s2w	{ SELECT a.id, a.v FROM la a WHERE a.id <= 8 AND a.v < (SELECT max(l.amt) FROM ll l WHERE l.acct = a.grp) ORDER BY a.id FOR UPDATE; }
step s2l	{ SELECT a.id, (SELECT max(l.amt) FROM ll l WHERE l.acct = a.grp) AS m FROM la a WHERE a.id >= 4 ORDER BY a.id LIMIT 3 FOR UPDATE; }
step s2lw	{ SELECT a.id, a.v, (SELECT max(l.amt) FROM ll l WHERE l.acct = a.grp) AS m FROM la a WHERE a.id >= 4 AND a.v < 60 ORDER BY a.id LIMIT 3 FOR SHARE; }

# the SubPlan again over the new grp; s1's ll row (committed with it) unseen by the statement's snapshot
permutation s2x s1g s2t s1c
permutation s1g s1i s2t s1c
# the qual over the SubPlan checked again: v = 100 fails it
permutation s1v s2w s1c
# under a LIMIT: the rechecked row counted, or skipped and the next row taken
permutation s1g s2l s1c
permutation s1v s2lw s1c
permutation s1g s2lw s1c

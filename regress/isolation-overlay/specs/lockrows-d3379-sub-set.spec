# lockrows D3379 SUBSET: a row lock over a Sort whose sequential scan carries a correlated SubPlan over an aggregate inner
# (LockRows over Sort over Seq Scan, the SubPlan in the scan's target list or qual). When s1 changed the row (READ
# COMMITTED), EvalPlanQual reruns the plan below LockRows over the newest version: the SubPlan runs again with the new
# row's arguments, under the statement's snapshot (s1's committed ll row stays unseen), and a qual over it is checked
# again. SKIP LOCKED passes over s1's locked row; NOWAIT raises. The recheck reruns the Sort's producer, whose SubPlan
# step reruns its set (rerun/epq/marks.rs sub_sets).
# Expected output is the C oracle's (PostgreSQL 18, pgdg).
setup
{
  CREATE TABLE la (id int PRIMARY KEY, grp int NOT NULL, v int NOT NULL);
  INSERT INTO la SELECT i, i % 4, i FROM generate_series(1, 40) i;
  CREATE TABLE ll (id int PRIMARY KEY, acct int NOT NULL, amt int NOT NULL);
  INSERT INTO ll SELECT i, i % 4, (i % 4) * 10 + i % 7 FROM generate_series(1, 200) i;
  CREATE INDEX ll_acct ON ll (acct);
  ANALYZE la, ll;
}

teardown
{
  DROP TABLE la, ll;
}

session s1
setup		{ BEGIN; }
step s1g	{ UPDATE la SET grp = 0 WHERE id = 5; }
step s1h	{ UPDATE la SET grp = 3 WHERE id = 5; }
step s1i	{ INSERT INTO ll VALUES (1000, 0, 99); }
step s1v	{ UPDATE la SET v = 100 WHERE id = 5; }
step s1d	{ DELETE FROM la WHERE id = 6; }
step s1k	{ SELECT id FROM la WHERE id = 3 FOR UPDATE; }
step s1c	{ COMMIT; }
teardown	{ ROLLBACK; }

session s2
setup		{ SET enable_indexscan = off; SET enable_bitmapscan = off; }
step s2x	{ EXPLAIN (COSTS OFF) SELECT a.id, a.grp, (SELECT max(l.amt) FROM ll l WHERE l.acct = a.grp) AS m FROM la a WHERE a.id <= 8 ORDER BY a.id FOR UPDATE; }
step s2t	{ SELECT a.id, a.grp, (SELECT max(l.amt) FROM ll l WHERE l.acct = a.grp) AS m FROM la a WHERE a.id <= 8 ORDER BY a.id FOR UPDATE; }
step s2w	{ SELECT a.id, a.v FROM la a WHERE a.id <= 8 AND a.v < (SELECT max(l.amt) FROM ll l WHERE l.acct = a.grp) ORDER BY a.id FOR UPDATE; }
step s2b	{ SELECT a.id, (SELECT count(*) FROM ll l WHERE l.acct = a.grp AND l.amt > a.v) AS c FROM la a WHERE a.id <= 8 AND (SELECT min(l.amt) FROM ll l WHERE l.acct = a.grp) < 25 ORDER BY a.id DESC FOR NO KEY UPDATE; }
step s2s	{ SELECT a.id, (SELECT sum(l.amt) FROM ll l WHERE l.acct = a.grp) AS s FROM la a WHERE a.id <= 6 ORDER BY a.id FOR UPDATE SKIP LOCKED; }
step s2n	{ SELECT a.id, (SELECT sum(l.amt) FROM ll l WHERE l.acct = a.grp) AS s FROM la a WHERE a.id <= 6 ORDER BY a.id FOR SHARE NOWAIT; }

# the SubPlan again over the new grp; s1's ll row (committed with it) unseen by the statement's snapshot
permutation s2x s1g s2t s1c
permutation s1g s1i s2t s1c
# the qual over the SubPlan checked again: v = 100 fails it
permutation s1v s2w s1c
# a SubPlan in the target list and one in the qual, both again (grp 3's min fails the qual); a deleted row gives none
permutation s1h s1d s2b s1c
permutation s1v s2b s1c
# SKIP LOCKED passes over s1's locked row; NOWAIT raises
permutation s1k s2s s1c
permutation s1k s2n s1c

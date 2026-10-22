# AGA-35 form 7 (D652-S1's lock half beside an InitPlan): a row lock over its own relation beside per-row SubPlan steps
# over an aggregate inner (lockrows-d652s-lock-subplan), `SELECT a.id, (SELECT max(l.amt) FROM sled l WHERE l.acct =
# a.grp) .. FOR UPDATE`, beside an InitPlan of the outer query that locks and writes nothing. Over a newer version (READ
# COMMITTED) EvalPlanQual reruns the scan's quals and every SubPlan step over the newest version under the statement's
# snapshot (a ledger row s1 changed is not seen), and copies the outer InitPlan's value (EvalPlanQualStart; a config row
# s1 changed does not move it, a VOLATILE one is called once), forcing a pending one its plan reads (s2e raises there).
# EvalPlanQualStart also initializes every SubPlan in the recheck's estate, re-arming an InitPlan of the subquery, which
# runs again (s2vi: a VOLATILE one is called again): Gravity keeps those refused (INIT_BELOW) and the row executor answers.
# Steps: a changed row, a changed SubPlan argument, a row failing the SubPlan's qual, a deleted row, SKIP LOCKED, NOWAIT
# 55P03, a changed config row, a changed ledger row, a VOLATILE InitPlan, a pending one in a CASE arm, a LIMIT above the
# lock, the subquery's own InitPlan. Expected output is the C oracle's (PostgreSQL 18, pgdg).
setup
{
  CREATE TABLE sacct (id int PRIMARY KEY, grp int NOT NULL, v int NOT NULL);
  INSERT INTO sacct SELECT i, i % 3, i * 10 FROM generate_series(1, 10) i;
  CREATE TABLE sled (id int PRIMARY KEY, acct int NOT NULL, amt int NOT NULL);
  INSERT INTO sled SELECT i, i % 4, i * 3 FROM generate_series(1, 40) i;
  CREATE INDEX sled_acct ON sled (acct);
  CREATE TABLE scfg (name text PRIMARY KEY, lo int NOT NULL, lim int NOT NULL);
  INSERT INTO scfg VALUES ('q', 20, 6);
  CREATE SEQUENCE ssq;
  ANALYZE sacct, sled, scfg;
}
teardown { DROP TABLE sacct, sled, scfg; DROP SEQUENCE ssq; }

session s1
setup		{ BEGIN; }
step s1a	{ UPDATE sacct SET v = v + 1 WHERE id = 3; }
step s1g	{ UPDATE sacct SET grp = grp + 1 WHERE id = 3; }
step s1v	{ UPDATE sacct SET v = 100000 WHERE id = 3; }
step s1d	{ DELETE FROM sacct WHERE id = 3; }
step s1l	{ SELECT id FROM sacct WHERE id = 3 FOR UPDATE; }
step s1q	{ UPDATE scfg SET lo = 1000, lim = 2 WHERE name = 'q'; }
step s1m	{ UPDATE sled SET amt = amt + 500 WHERE acct = 1; }
step s1c	{ COMMIT; }
teardown	{ ROLLBACK; }

session s2
setup		{ SET enable_seqscan = off; SET enable_bitmapscan = off; }
step s2f	{ SELECT a.id, a.v, (SELECT max(l.amt) FROM sled l WHERE l.acct = a.grp) AS m FROM sacct a WHERE a.id <= 6 AND a.v >= (SELECT lo FROM scfg WHERE name = 'q') ORDER BY a.id FOR UPDATE; }
step s2q	{ SELECT a.id, a.v FROM sacct a WHERE a.id <= (SELECT lim FROM scfg WHERE name = 'q') AND a.v < (SELECT sum(l.amt) FROM sled l WHERE l.acct = a.grp) ORDER BY a.id FOR NO KEY UPDATE; }
step s2sk	{ SELECT a.id, (SELECT max(l.amt) FROM sled l WHERE l.acct = a.grp) AS m FROM sacct a WHERE a.id <= (SELECT lim FROM scfg WHERE name = 'q') ORDER BY a.id FOR UPDATE SKIP LOCKED; }
step s2nw	{ SELECT a.id, (SELECT max(l.amt) FROM sled l WHERE l.acct = a.grp) AS m FROM sacct a WHERE a.id <= (SELECT lim FROM scfg WHERE name = 'q') ORDER BY a.id FOR UPDATE NOWAIT; }
step s2v	{ SELECT a.id, (SELECT max(l.amt) FROM sled l WHERE l.acct = a.grp) AS m FROM sacct a WHERE a.id <= (SELECT nextval('ssq') * 0 + 6) ORDER BY a.id FOR UPDATE; }
step s2e	{ SELECT a.id, (SELECT max(l.amt) FROM sled l WHERE l.acct = a.grp) AS m FROM sacct a WHERE a.id <= 6 AND CASE WHEN a.v > 100000 THEN a.v > (SELECT 1 / (lim - lim) FROM scfg WHERE name = 'q') ELSE true END ORDER BY a.id FOR UPDATE; }
step s2l	{ SELECT a.id, (SELECT max(l.amt) FROM sled l WHERE l.acct = a.grp) AS m FROM sacct a WHERE a.id > 1 AND a.v >= (SELECT lo FROM scfg WHERE name = 'q') ORDER BY a.id LIMIT 3 FOR UPDATE; }
step s2i	{ SELECT a.id, (SELECT max(l.amt) FROM sled l WHERE l.acct = a.grp AND l.amt < (SELECT lim * 10 FROM scfg WHERE name = 'q')) AS m FROM sacct a WHERE a.id <= 6 ORDER BY a.id FOR UPDATE; }
step s2vi	{ SELECT a.id, (SELECT max(l.amt) FROM sled l WHERE l.acct = a.grp AND l.amt > (SELECT nextval('ssq') * 0 - 1)) AS m FROM sacct a WHERE a.id <= 6 ORDER BY a.id FOR UPDATE; }
step s2seq	{ SELECT last_value, is_called FROM ssq; }

permutation s1a s2f s1c
permutation s1g s2f s1c
permutation s1v s2q s1c
permutation s1d s2f s1c
permutation s1l s2sk s1c
permutation s1l s2nw s1c
permutation s1q s1a s2f s1c
permutation s1q s1a s2q s1c
permutation s1m s1g s2f s1c
permutation s1a s2v s1c s2seq
permutation s1a s2e s1c
permutation s1a s2l s1c
permutation s1a s2i s1c
permutation s1a s2vi s1c s2seq

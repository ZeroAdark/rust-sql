# AGA-35 form 6 (lockrows SUBLOCK beside an InitPlan): a LockRows inside a correlated EXISTS SubPlan (lockrows-sublock),
# `.. WHERE EXISTS (SELECT 1 FROM sjob k WHERE k.id = j.id .. FOR UPDATE)`, beside an InitPlan of the outer query that locks
# and writes nothing. The SubPlan's LockRows locks the row its first pull reaches; over a newer version (READ COMMITTED) its
# own EvalPlanQual reruns the inner scan over the newest version, which reads no InitPlan of the outer query. The
# statement's UPDATE then meets the row s1 changed: its own EvalPlanQual copies the outer InitPlan's value
# (EvalPlanQualStart; a config row s1 changed does not move it), forcing a pending one its plan reads (s2e raises there).
# EvalPlanQualStart also initializes every SubPlan in the recheck's estate, re-arming an InitPlan of the subquery, which
# runs again where the recheck reads it (s3v, s2vi: a VOLATILE one is called again, once per EvalPlanQual state: the inner
# lock's, the write's, and the inner lock's inside the write's recheck), and forces a pending one its subplan reads (s3c
# raises there). D3274: Gravity serves these, its IR recheck forcing and arming as PostgreSQL's states do. Steps: a changed row, a row failing the inner's qual, a deleted row, SKIP LOCKED, NOWAIT 55P03,
# another row locked through j.ref, a changed config row, a VOLATILE outer InitPlan called once (beside the write and in
# the SELECT), which rows the SELECT form holds (s4, its locks held open: s1's NOWAIT on a locked row fails, on a done row
# succeeds). Expected output is the C oracle's (PostgreSQL 18, pgdg).
setup
{
  CREATE TABLE sjob (id int PRIMARY KEY, state text NOT NULL, prio int NOT NULL, ref int, attempts int NOT NULL DEFAULT 0);
  INSERT INTO sjob SELECT i, CASE WHEN i % 4 = 0 THEN 'done' ELSE 'ready' END, CASE WHEN i % 2 = 1 THEN 9 ELSE 1 END, i + 4, 0 FROM generate_series(1, 10) i;
  CREATE INDEX sjob_ready ON sjob (prio, id) WHERE state = 'ready';
  CREATE TABLE scfg (name text PRIMARY KEY, prio int NOT NULL, state text NOT NULL, lim int NOT NULL);
  INSERT INTO scfg VALUES ('q', 9, 'ready', 8);
  CREATE SEQUENCE ssq;
  ANALYZE sjob, scfg;
}
teardown { DROP TABLE sjob, scfg; DROP SEQUENCE ssq; }

session s1
setup		{ BEGIN; }
step s1a	{ UPDATE sjob SET attempts = attempts + 100 WHERE id = 5; }
step s1s	{ UPDATE sjob SET state = 'done' WHERE id = 5; }
step s1p	{ UPDATE sjob SET prio = 2 WHERE id = 5; }
step s1d	{ DELETE FROM sjob WHERE id = 5; }
step s1l	{ SELECT id FROM sjob WHERE id = 5 FOR UPDATE; }
step s1j	{ UPDATE sjob SET attempts = attempts + 100 WHERE id = 1; }
step s1r	{ UPDATE sjob SET ref = 99 WHERE id = 1; }
step s1q	{ UPDATE scfg SET prio = 1, state = 'done', lim = 2 WHERE name = 'q'; }
step s1n3	{ SELECT id FROM sjob WHERE id = 3 FOR UPDATE NOWAIT; }
step s1n4	{ SELECT id FROM sjob WHERE id = 4 FOR UPDATE NOWAIT; }
step s1c	{ COMMIT; }
teardown	{ ROLLBACK; }

session s2
setup		{ SET enable_seqscan = off; SET enable_bitmapscan = off; }
step s2u	{ UPDATE sjob j SET attempts = j.attempts + 1 WHERE j.id <= (SELECT lim FROM scfg WHERE name = 'q') AND EXISTS (SELECT 1 FROM sjob k WHERE k.id = j.id AND k.state = 'ready' AND k.prio = 9 FOR UPDATE) RETURNING j.id, j.attempts; }
step s2sk	{ UPDATE sjob j SET attempts = j.attempts + 1 WHERE j.id <= (SELECT lim FROM scfg WHERE name = 'q') AND EXISTS (SELECT 1 FROM sjob k WHERE k.id = j.id AND k.state = 'ready' AND k.prio = 9 FOR UPDATE SKIP LOCKED) RETURNING j.id, j.attempts; }
step s2nw	{ UPDATE sjob j SET attempts = j.attempts + 1 WHERE j.id <= (SELECT lim FROM scfg WHERE name = 'q') AND EXISTS (SELECT 1 FROM sjob k WHERE k.id = j.id AND k.state = 'ready' AND k.prio = 9 FOR UPDATE NOWAIT) RETURNING j.id, j.attempts; }
step s2w	{ UPDATE sjob j SET attempts = j.attempts + 1 WHERE j.id <= (SELECT lim FROM scfg WHERE name = 'q') - 4 AND EXISTS (SELECT 1 FROM sjob k WHERE k.id = j.ref AND k.state = 'ready' FOR UPDATE) RETURNING j.id, j.attempts; }
step s2i	{ UPDATE sjob j SET attempts = j.attempts + 1 WHERE j.id <= 8 AND EXISTS (SELECT 1 FROM sjob k WHERE k.id = j.id AND k.state = (SELECT state FROM scfg WHERE name = 'q') AND k.prio = (SELECT prio FROM scfg WHERE name = 'q') FOR UPDATE) RETURNING j.id, j.attempts; }
step s2v	{ UPDATE sjob j SET attempts = j.attempts + 1 WHERE j.id <= (SELECT nextval('ssq') * 0 + 8) AND EXISTS (SELECT 1 FROM sjob k WHERE k.id = j.id AND k.state = 'ready' AND k.prio = 9 FOR UPDATE) RETURNING j.id, j.attempts; }
step s2vi	{ UPDATE sjob j SET attempts = j.attempts + 1 WHERE j.id <= 8 AND EXISTS (SELECT 1 FROM sjob k WHERE k.id = j.id AND k.state = 'ready' AND k.prio > (SELECT nextval('ssq') * 0) FOR UPDATE) RETURNING j.id, j.attempts; }
step s2e	{ UPDATE sjob j SET attempts = j.attempts + 1 WHERE j.id <= 8 AND CASE WHEN j.prio > 100 THEN j.attempts > (SELECT 1 / (lim - lim) FROM scfg WHERE name = 'q') ELSE true END AND EXISTS (SELECT 1 FROM sjob k WHERE k.id = j.id AND k.state = 'ready' AND k.prio = 9 FOR UPDATE) RETURNING j.id, j.attempts; }
step s2seq	{ SELECT last_value, is_called FROM ssq; }

session s3
setup		{ SET enable_seqscan = off; SET enable_bitmapscan = off; }
step s3o	{ SELECT j.id FROM sjob j WHERE j.id <= (SELECT lim FROM scfg WHERE name = 'q') AND EXISTS (SELECT 1 FROM sjob k WHERE k.id = j.id AND k.state = 'ready' FOR SHARE); }
step s3ok	{ SELECT j.id FROM sjob j WHERE j.id <= (SELECT lim FROM scfg WHERE name = 'q') AND EXISTS (SELECT 1 FROM sjob k WHERE k.id = j.id AND k.state = 'ready' AND k.prio = 9 FOR UPDATE SKIP LOCKED); }
step s3ov	{ SELECT j.id FROM sjob j WHERE j.id <= 8 AND j.prio > (SELECT nextval('ssq') * 0) AND EXISTS (SELECT 1 FROM sjob k WHERE k.id = j.id AND k.state = 'ready' FOR UPDATE); }
step s3l	{ SELECT j.id FROM sjob j WHERE j.id <= 8 AND EXISTS (SELECT 1 FROM sjob k WHERE k.id = j.id AND k.state = (SELECT state FROM scfg WHERE name = 'q') FOR SHARE); }
step s3k	{ SELECT j.id FROM sjob j WHERE j.id <= (SELECT lim FROM scfg WHERE name = 'q') AND EXISTS (SELECT 1 FROM sjob k WHERE k.id = j.id AND k.state = 'ready' AND k.prio = (SELECT prio FROM scfg WHERE name = 'q') FOR UPDATE SKIP LOCKED); }
step s3v	{ SELECT j.id FROM sjob j WHERE j.id <= 8 AND EXISTS (SELECT 1 FROM sjob k WHERE k.id = j.id AND k.state = 'ready' AND k.prio > (SELECT nextval('ssq') * 0) FOR UPDATE); }
step s3c	{ SELECT j.id FROM sjob j WHERE j.id <= 8 AND EXISTS (SELECT 1 FROM sjob k WHERE k.id = j.id AND k.state = 'ready' AND CASE WHEN k.prio > 100 THEN k.attempts > (SELECT 1 / (lim - lim) FROM scfg WHERE name = 'q') ELSE true END FOR UPDATE); }
step s3seq	{ SELECT last_value, is_called FROM ssq; }

session s4
setup		{ SET enable_seqscan = off; SET enable_bitmapscan = off; }
step s4b	{ BEGIN; }
step s4l	{ SELECT j.id FROM sjob j WHERE j.id <= (SELECT lim FROM scfg WHERE name = 'q') - 4 AND EXISTS (SELECT 1 FROM sjob k WHERE k.id = j.id + 2 AND k.state = 'ready' FOR UPDATE); }
step s4c	{ COMMIT; }

# ready: every id but 4 and 8; prio 9: the odd ids; j.ref = j.id + 4 (8 is done, so s2w takes 1, 2, 3)
permutation s1a s2u s1c
permutation s1s s2u s1c
permutation s1p s2u s1c
permutation s1d s2u s1c
permutation s1l s2sk s1c
permutation s1l s2nw s1c
permutation s1j s2w s1c
permutation s1r s2w s1c
permutation s1q s1a s2u s1c
permutation s1q s1a s2i s1c
permutation s1a s2v s1c s2seq
permutation s1a s2vi s1c s2seq
permutation s1a s2e s1c
permutation s1a s3o s1c
permutation s1s s3o s1c
permutation s1q s1a s3o s1c
permutation s1l s3ok s1c
permutation s1a s3ov s1c s3seq
permutation s1a s3l s1c
permutation s1q s1a s3l s1c
permutation s1l s3k s1c
permutation s1a s3v s1c s3seq
permutation s1a s3c s1c
permutation s4b s4l s1n3 s1n4 s4c

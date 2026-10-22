# D4204 (Phase 0c CE-1): a LIMIT .. FOR UPDATE subquery under a Materialize (the D3718 form: LockRows -> Nested Loop ->
# (Subquery s -> Limit -> LockRows -> Index Scan; Materialize -> Subquery w -> Limit -> LockRows -> Index Scan)).
# PostgreSQL's Material pulls one child row per fetch, so the first joined row (s1, w1) is sent (printtup's conversion)
# before w2 is locked. Under client_encoding LATIN1 w1's name 'a-€' raises 22P05 there: s2 holds w2, and its later lock
# on s1 goes through at once. A fill run whole at the first outer row locks w2 first: NOWAIT raises 55P03, and waiting on
# w2 while s2 asks for s1 deadlocks (40P01). s3 (UTF8) reads the six rows. Expected output is PostgreSQL 18.6's.
setup
{
  CREATE TABLE lmj (id int PRIMARY KEY, state text NOT NULL, prio int NOT NULL);
  INSERT INTO lmj SELECT g, CASE WHEN g % 4 = 0 THEN 'ready' WHEN g % 4 = 1 THEN 'done' ELSE 'running' END, (g * 7) % 5 FROM generate_series(1, 200) g;
  CREATE INDEX lmj_state_id ON lmj (state, id);
  CREATE TABLE lmw (id int PRIMARY KEY, name text NOT NULL, team int NOT NULL);
  INSERT INTO lmw SELECT g, CASE WHEN g = 1 THEN 'a-€' ELSE 'worker-' || g END, g % 3 FROM generate_series(1, 30) g;
  ANALYZE lmj, lmw;
}
teardown { DROP TABLE lmj, lmw; }

session s1
setup		{ SET client_encoding = 'LATIN1'; SET enable_seqscan = off; SET enable_bitmapscan = off; }
step s1nw	{ SELECT s.id, w.id, w.name FROM (SELECT id FROM lmj WHERE state = 'ready' ORDER BY id LIMIT 3) s, (SELECT id, name FROM lmw ORDER BY id LIMIT 2) w FOR UPDATE NOWAIT; }
step s1w	{ SELECT s.id, w.id, w.name FROM (SELECT id FROM lmj WHERE state = 'ready' ORDER BY id LIMIT 3) s, (SELECT id, name FROM lmw ORDER BY id LIMIT 2) w FOR UPDATE; }

session s2
step s2b	{ BEGIN; }
step s2w2	{ SELECT id FROM lmw WHERE id = 2 FOR UPDATE; }
step s2s1	{ SELECT id FROM lmj WHERE id = 4 FOR UPDATE; }
step s2c	{ COMMIT; }

session s3
setup		{ SET enable_seqscan = off; SET enable_bitmapscan = off; }
step s3q	{ SELECT s.id, w.id, w.name FROM (SELECT id FROM lmj WHERE state = 'ready' ORDER BY id LIMIT 3) s, (SELECT id, name FROM lmw ORDER BY id LIMIT 2) w FOR UPDATE; }

# ready ids by id: 4, 8, 12; w by id: 1 ('a-€'), 2
permutation s2b s2w2 s1nw s2c
permutation s2b s2w2 s1w s2s1 s2c
permutation s3q

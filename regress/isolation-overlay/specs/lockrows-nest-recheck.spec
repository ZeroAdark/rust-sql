# lockrows NEST (lk-0085, lk-0072, lk-0073 forms): a row lock over an INNER nested loop whose inner is one heap scan
# (bitmap, index, or a plain seq scan rescanned per outer row) of a marked relation. PostgreSQL's EvalPlanQual reruns the
# plan below LockRows with every marked relation's test tuple substituted: the locked rows' newest versions, a REFERENCE
# row refetched at the ctid the join read (EvalPlanQualFetchRowMark, the version it read, not the newest), and the nest's
# inner rescanned over its test tuple, rechecked by its keys (IndexRecheck, BitmapHeapRecheck) or its filter. Forms: the
# outer locked and the inner REFERENCE-marked (FOR UPDATE OF a: the inner's row refetched as read, even after a
# concurrent update of it), the inner locked under a REFERENCE-marked outer (FOR UPDATE OF l: its newest version, skipped
# when it no longer meets the inner's key), both locked under an ORDER BY (LockRows over a Sort over the nest), and
# the inner as a bitmap, an index and a seq scan. Expected output is the C oracle's (PostgreSQL 18, pgdg).
setup
{
  CREATE TABLE nra (id int PRIMARY KEY, bal int NOT NULL);
  INSERT INTO nra SELECT i, i * 10 FROM generate_series(1, 200) i;
  CREATE TABLE nrl (id int PRIMARY KEY, acct int NOT NULL, kind text NOT NULL, amt int NOT NULL);
  INSERT INTO nrl SELECT i, (i % 200) + 1, CASE WHEN (i / 200) % 2 = 0 THEN 'cr' ELSE 'db' END, i FROM generate_series(1, 2000) i;
  CREATE INDEX nrl_acct ON nrl (acct);
  CREATE TABLE nrc (id int PRIMARY KEY, l_id int NOT NULL);
  INSERT INTO nrc SELECT i, i * 3 FROM generate_series(1, 900) i;
  CREATE INDEX nrc_l ON nrc (l_id);
  ANALYZE nra, nrl, nrc;
}

teardown
{
  DROP TABLE nra, nrl, nrc;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1a2	{ UPDATE nra SET bal = bal + 1 WHERE id = 2; }
step s1a2k	{ UPDATE nra SET id = id + 1000 WHERE id = 2; }
step s1l2	{ UPDATE nrl SET amt = amt + 5000 WHERE id = 401; }
step s1l2m	{ UPDATE nrl SET acct = 3 WHERE id = 401; }
step s1l5	{ UPDATE nrl SET amt = amt + 5000 WHERE id = 404; }
step s1l5m	{ UPDATE nrl SET acct = 6 WHERE id = 404; }
step s1l5d	{ DELETE FROM nrl WHERE id = 804; }
step s1a7	{ UPDATE nra SET bal = bal + 1 WHERE id = 7; }
step s1l8m	{ UPDATE nrl SET acct = 9 WHERE id = 407; }
step s1l2t	{ UPDATE nrl SET amt = amt + 5000 WHERE id = 201; }
step s1c3	{ UPDATE nrc SET l_id = 7 WHERE id = 134; }
step s1c	{ COMMIT; }
teardown	{ ROLLBACK; }

session s2
setup		{ SET enable_hashjoin = off; SET enable_mergejoin = off; RESET enable_seqscan; RESET enable_indexscan; RESET enable_bitmapscan; RESET enable_material; }
step s2b	{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s2bm	{ SET enable_indexscan = off; }
step s2nos	{ SET enable_seqscan = off; }
step s2ix	{ SET enable_seqscan = off; SET enable_bitmapscan = off; }
step s2seq	{ SET enable_indexscan = off; SET enable_bitmapscan = off; SET enable_material = off; }
step s2xa	{ EXPLAIN (COSTS OFF) SELECT a.id, a.bal, l.id, l.amt FROM nra a JOIN nrl l ON l.acct = a.id WHERE a.id <= 3 AND l.kind = 'cr' FOR UPDATE OF a; }
step s2a	{ SELECT a.id, a.bal, l.id, l.amt FROM nra a JOIN nrl l ON l.acct = a.id WHERE a.id <= 3 AND l.kind = 'cr' FOR UPDATE OF a; }
step s2xl	{ EXPLAIN (COSTS OFF) SELECT a.id, l.id, l.acct, l.amt FROM nra a JOIN nrl l ON l.acct = a.id WHERE a.id = 5 FOR UPDATE OF l; }
step s2l	{ SELECT a.id, l.id, l.acct, l.amt FROM nra a JOIN nrl l ON l.acct = a.id WHERE a.id = 5 FOR UPDATE OF l; }
step s2xs	{ EXPLAIN (COSTS OFF) SELECT a.id, a.bal, l.id, l.acct FROM nra a JOIN nrl l ON l.acct = a.id WHERE a.id IN (7, 8) ORDER BY l.id FOR UPDATE; }
step s2s	{ SELECT a.id, a.bal, l.id, l.acct FROM nra a JOIN nrl l ON l.acct = a.id WHERE a.id IN (7, 8) ORDER BY l.id FOR UPDATE; }
step s2xt	{ EXPLAIN (COSTS OFF) SELECT a.id, l.id, l.amt, c.id FROM nra a JOIN nrl l ON l.acct = a.id JOIN nrc c ON c.l_id = l.id WHERE a.id <= 3 FOR UPDATE OF l; }
step s2t	{ SELECT a.id, l.id, l.amt, c.id FROM nra a JOIN nrl l ON l.acct = a.id JOIN nrc c ON c.l_id = l.id WHERE a.id <= 3 FOR UPDATE OF l; }
step s2ta	{ SELECT a.id, l.id, c.id, c.l_id FROM nra a JOIN nrl l ON l.acct = a.id JOIN nrc c ON c.l_id = l.id WHERE a.id <= 3 FOR UPDATE; }
step s2xv	{ EXPLAIN (COSTS OFF) SELECT a.id, a.bal FROM nra a JOIN (VALUES (2), (5), (9)) v(k) ON v.k = a.id FOR UPDATE OF a; }
step s2v	{ SELECT a.id, a.bal FROM nra a JOIN (VALUES (2), (5), (9)) v(k) ON v.k = a.id FOR UPDATE OF a; }
step s2c	{ COMMIT; }

# outer locked, inner REFERENCE (bitmap inner): no concurrency, then a's newest version
permutation s2bm s2b s2xa s2a s2c
permutation s2bm s2b s1a2 s2a s1c s2c
# a's key moved: the recheck finds no row for a = 2
permutation s2bm s2b s1a2k s2a s1c s2c
# the REFERENCE row updated too: its version as the join read it (the old amt), a's newest
permutation s2bm s2b s1l2 s1a2 s2a s1c s2c
# the REFERENCE row moved to another account: refetched as read, still joined
permutation s2bm s2b s1l2m s1a2 s2a s1c s2c
# the inner by its index, and as a seq scan rescanned per outer row
permutation s2ix s2b s2xa s1l2 s1a2 s2a s1c s2c
permutation s2seq s2b s2xa s1l2 s1a2 s2a s1c s2c
# the inner locked under a REFERENCE outer (a seq scan inner): its newest version, skipped once it leaves the key or
# is deleted
permutation s2b s2xl s1l5 s2l s1c s2c
permutation s2b s1l5m s2l s1c s2c
permutation s2b s1l5d s2l s1c s2c
# the same by a bitmap inner and by an index inner
permutation s2nos s2b s2xl s1l5 s2l s1c s2c
permutation s2nos s2b s1l5m s2l s1c s2c
permutation s2ix s2b s2xl s1l5m s2l s1c s2c
# both locked under an ORDER BY (LockRows over a Sort over the nest), a seq scan inner and a bitmap one
permutation s2b s2xs s1a7 s2s s1c s2c
permutation s2b s1l8m s2s s1c s2c
permutation s2nos s2b s2xs s1l8m s2s s1c s2c
# three relations over two nests: the middle one locked (its newest version, the third rescanned over its REFERENCE row),
# and all three locked (the third's row moved off its key: skipped)
permutation s2b s2xt s1l2t s2t s1c s2c
permutation s2b s1c3 s2ta s1c s2c
# a VALUES outer (its COPY row handed back whole) over the locked relation's index inner (lk-0104): a's newest version,
# and a's key moved (skipped)
permutation s2nos s2b s2xv s1a2 s2v s1c s2c
permutation s2nos s2b s1a2k s2v s1c s2c

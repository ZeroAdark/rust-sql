# lockrows REFBUILD: a row lock of a partitioned or inheritance family whose leaves are on the build side of an inner hash
# join probed by a REFERENCE-marked relation (LockRows -> Hash Join -> (Seq Scan rba, Hash -> Append -> the leaves), FOR
# ... OF the family). When s1 changed a locked row (READ COMMITTED), EvalPlanQual reruns the join over the newest version
# in its own leaf (the other leaves' marks inactive) and the rba row refetched at its junk ctid (EvalPlanQualFetchRowMark
# under SnapshotAny: the version the join read, even when s1 updated it too): a row whose newest version still joins is
# returned with its new values; one whose newest version fails the filter, whose join key moved (the refetched rba row is
# the old key's), or that was deleted is skipped; a row of another leaf is rechecked in its own leaf; SKIP LOCKED skips a
# row s1 holds. D3379 REFCALM (s2z): a probe-side qual that can raise (an integer division) but calls only builtin
# functions is redone over the refetched rba row, the version the join read (s1r's new bal would fail it). Expected
# output is the C oracle's (PostgreSQL 18, pgdg).
setup
{
  CREATE TABLE rba (id int PRIMARY KEY, owner text NOT NULL, bal int NOT NULL);
  INSERT INTO rba SELECT i, 'o' || (i % 12), (i * 37) % 101 FROM generate_series(1, 2000) i;
  CREATE TABLE rbp (id int NOT NULL, v int NOT NULL, note text NOT NULL) PARTITION BY RANGE (id);
  CREATE TABLE rbp_a PARTITION OF rbp FOR VALUES FROM (1) TO (100);
  CREATE TABLE rbp_b PARTITION OF rbp FOR VALUES FROM (100) TO (200);
  CREATE TABLE rbp_d PARTITION OF rbp DEFAULT;
  INSERT INTO rbp SELECT i, i % 50, 'n' || i FROM generate_series(1, 300) i;
  CREATE TABLE rbh (id int NOT NULL, v int NOT NULL, note text NOT NULL);
  CREATE TABLE rbh1 () INHERITS (rbh);
  INSERT INTO rbh SELECT i, i % 40, 'h' || i FROM generate_series(1, 120) i;
  INSERT INTO rbh1 SELECT i, i % 40, 'c' || i FROM generate_series(121, 240) i;
  ANALYZE rba, rbp, rbh, rbh1;
}
teardown { DROP TABLE rba, rbp, rbh1, rbh; }

session s1
setup		{ BEGIN; }
step s1u	{ UPDATE rbp SET note = note || '+' WHERE id = 51; }
step s1v	{ UPDATE rbp SET v = 40 WHERE id = 51; }
step s1k	{ UPDATE rbp SET id = 53 WHERE id = 51; }
step s1d	{ DELETE FROM rbp WHERE id = 51; }
step s1r	{ UPDATE rba SET bal = bal + 1000 WHERE id = 51; }
step s1b	{ UPDATE rbp SET note = note || '*', v = 2 WHERE id = 151; }
step s1h	{ UPDATE rbh SET note = note || '!' WHERE id IN (41, 161); }
step s1hv	{ UPDATE rbh SET v = 9 WHERE id = 161; }
step s1l	{ SELECT id FROM rbp WHERE id = 51 FOR UPDATE; }
step s1c	{ COMMIT; }
teardown	{ ROLLBACK; }

session s2
setup		{ SET enable_nestloop = off; SET enable_mergejoin = off; }
step s2x	{ EXPLAIN (COSTS OFF) SELECT p.id, p.v, p.note, a.bal FROM rbp p JOIN rba a ON a.id = p.id WHERE p.v < 3 FOR UPDATE OF p; }
step s2l	{ SELECT p.id, p.v, p.note, a.bal FROM rbp p JOIN rba a ON a.id = p.id WHERE p.v < 3 FOR UPDATE OF p; }
step s2s	{ SELECT p.id, p.note, a.owner FROM rbp p JOIN rba a ON a.id = p.id WHERE p.v < 3 AND a.bal < 90 FOR SHARE OF p; }
step s2sk	{ SELECT p.id, p.note FROM rbp p JOIN rba a ON a.id = p.id WHERE p.v < 3 FOR UPDATE OF p SKIP LOCKED; }
step s2z	{ SELECT p.id, p.v, p.note, a.bal FROM rbp p JOIN rba a ON a.id = p.id WHERE p.v < 3 AND 1000 / (a.bal + 1) > 5 FOR UPDATE OF p; }
step s2hx	{ EXPLAIN (COSTS OFF) SELECT h.id, h.note, a.bal FROM rbh h JOIN rba a ON a.id = h.id WHERE h.v = 1 FOR NO KEY UPDATE OF h; }
step s2h	{ SELECT h.id, h.note, a.bal FROM rbh h JOIN rba a ON a.id = h.id WHERE h.v = 1 FOR NO KEY UPDATE OF h; }

# v < 3: 1 2 50 51 52 100 101 102 150 151 152 200 201 202 250 251 252 300 (rbp_a / rbp_b / rbp_d); v = 1 in rbh: 1 41 81
# (the parent's own rows) 121 161 201 (rbh1)
permutation s2x s2l s2hx
permutation s1u s2l s1c
permutation s1v s2l s1c
permutation s1k s2l s1c
permutation s1d s2l s1c
permutation s1r s1u s2l s1c
permutation s1r s1u s2s s1c
permutation s1b s2l s1c
permutation s1l s2sk s1c
permutation s1h s2h s1c
permutation s1hv s2h s1c
permutation s1u s2z s1c
permutation s1r s1u s2z s1c

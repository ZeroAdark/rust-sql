# lockrows FAMJOIN: a row lock on a partitioned / inheritance family joined to another relation (FOR UPDATE OF the family:
# the other relation REFERENCE-marked). ExecLockRows locks each row in the leaf its tableoid names (the other children's
# marks inactive); EvalPlanQual reruns the join over that leaf's newest version and the other relation's row as it was
# read (EvalPlanQualFetchRowMark refetches a REFERENCE row at its junk ctid). Forms: LockRows over a hash join whose outer
# is the family's Append (a concurrent update of a family row kept in the quals, one moving it out of the quals, one moving
# its join key, an update of the other relation's row: the version read is joined), the inheritance tree's own rows, the
# family pruned to one leaf, FOR SHARE OF with SKIP LOCKED past a held row, the join under a Sort (ORDER BY), and the
# family on the hash join's build side. Expected output is the C oracle's (PostgreSQL 18, pgdg).
setup
{
  CREATE TABLE jo (id int PRIMARY KEY, bal int NOT NULL, tag text);
  INSERT INTO jo SELECT g, g * 100, 't' || g FROM generate_series(1, 40) g;
  CREATE TABLE jp (d int NOT NULL, id int NOT NULL, v int) PARTITION BY RANGE (d);
  CREATE TABLE jp1 PARTITION OF jp FOR VALUES FROM (0) TO (10);
  CREATE TABLE jp2 (v int, gone int, id int NOT NULL, d int NOT NULL);
  ALTER TABLE jp2 DROP COLUMN gone;
  ALTER TABLE jp ATTACH PARTITION jp2 FOR VALUES FROM (10) TO (20);
  INSERT INTO jp SELECT g % 20, g, g FROM generate_series(1, 30) g;
  CREATE TABLE jh (id int, v int);
  CREATE TABLE jh_c (extra text) INHERITS (jh);
  INSERT INTO jh SELECT g, g FROM generate_series(1, 5) g;
  INSERT INTO jh_c SELECT g, g, 'x' FROM generate_series(6, 12) g;
  ANALYZE jo, jp, jh, jh_c;
}

teardown
{
  DROP TABLE jo, jp;
  DROP TABLE jh CASCADE;
}

session s1
step s1b	{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1u	{ UPDATE jp SET v = v + 1 WHERE id = 12; }
step s1m	{ UPDATE jp SET v = 99 WHERE id = 12; }
step s1k	{ UPDATE jp SET id = 31 WHERE id = 12; }
step s1o	{ UPDATE jo SET bal = -1 WHERE id = 12; }
step s1h	{ UPDATE jh SET v = v + 10 WHERE id IN (3, 8); }
step s1hm	{ UPDATE jh SET v = 99 WHERE id = 8; }
step s1hk	{ UPDATE jh SET id = 40 WHERE id = 3; }
step s1l	{ SELECT id FROM jp WHERE id = 12 FOR UPDATE; }
step s1c	{ COMMIT; }

session s2
setup		{ SET enable_nestloop = off; SET enable_mergejoin = off; }
step s2b	{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s2x	{ EXPLAIN (COSTS OFF) SELECT p.tableoid::regclass, p.id, p.v, o.bal FROM jp p JOIN jo o ON o.id = p.id WHERE p.v < 50 AND o.tag IN ('t9', 't12') FOR UPDATE OF p; }
step s2j	{ SELECT p.tableoid::regclass, p.id, p.v, o.bal FROM jp p JOIN jo o ON o.id = p.id WHERE p.v < 50 AND o.tag IN ('t9', 't12') FOR UPDATE OF p; }
step s2y	{ EXPLAIN (COSTS OFF) SELECT p.tableoid::regclass, p.id, p.v, o.bal FROM jp p JOIN jo o ON o.id = p.id WHERE p.v < 50 AND p.id IN (9, 12) FOR UPDATE OF p; }
step s2k	{ SELECT p.tableoid::regclass, p.id, p.v, o.bal FROM jp p JOIN jo o ON o.id = p.id WHERE p.v < 50 AND p.id IN (9, 12) FOR UPDATE OF p; }
step s2o	{ SELECT p.tableoid::regclass, p.id, p.v, o.bal FROM jp p JOIN jo o ON o.id = p.id WHERE p.v < 50 AND o.tag IN ('t8', 't9', 't11', 't12', 't13') ORDER BY p.id FOR UPDATE OF p; }
step s2w	{ EXPLAIN (COSTS OFF) SELECT h.tableoid::regclass, h.id, h.v, o.tag FROM jh h JOIN jo o ON o.id = h.id WHERE h.v < 50 AND o.tag IN ('t3', 't8') FOR UPDATE OF h; }
step s2h	{ SELECT h.tableoid::regclass, h.id, h.v, o.tag FROM jh h JOIN jo o ON o.id = h.id WHERE h.v < 50 AND o.tag IN ('t3', 't8') FOR UPDATE OF h; }
step s2q	{ EXPLAIN (COSTS OFF) SELECT p.tableoid::regclass, p.id, p.v, o.bal FROM jp p JOIN jo o ON o.id = p.id WHERE p.d >= 10 AND p.v < 50 AND o.tag IN ('t9', 't12') FOR UPDATE OF p; }
step s2p	{ SELECT p.tableoid::regclass, p.id, p.v, o.bal FROM jp p JOIN jo o ON o.id = p.id WHERE p.d >= 10 AND p.v < 50 AND o.tag IN ('t9', 't12') FOR UPDATE OF p; }
step s2t	{ EXPLAIN (COSTS OFF) SELECT p.tableoid::regclass, p.id, p.v, o.bal FROM jp p JOIN jo o ON o.id = p.id WHERE p.d = 12 AND p.v < 50 AND o.tag IN ('t9', 't12') FOR UPDATE OF p; }
step s2r	{ SELECT p.tableoid::regclass, p.id, p.v, o.bal FROM jp p JOIN jo o ON o.id = p.id WHERE p.d = 12 AND p.v < 50 AND o.tag IN ('t9', 't12') FOR UPDATE OF p; }
step s2s	{ SELECT p.tableoid::regclass, p.id, o.bal FROM jp p JOIN jo o ON o.id = p.id WHERE o.tag IN ('t11', 't12', 't13') FOR SHARE OF p SKIP LOCKED; }
step s2c	{ COMMIT; }

# the family's union probing the other relation's build (the served form): kept in place, out of the quals, the join key
# moved, the other relation's row updated too (joined as read)
permutation s2b s2x s1b s1u s2j s1c s2c
permutation s2b s1b s1m s2j s1c s2c
permutation s2b s1b s1k s2j s1c s2c
permutation s2b s1b s1o s1u s2j s1c s2c
# the family on the build side (another relation's scan probing a build of the family's union): refused by name (D652)
permutation s2b s2y s1b s1m s2k s1c s2c
# the inheritance tree: its own rows and a child's, kept in place, out of the quals, the join key moved, both
permutation s2b s2w s1b s1h s2h s1c s2c
permutation s2b s1b s1hm s2h s1c s2c
permutation s2b s1b s1hk s2h s1c s2c
permutation s2b s1b s1hm s1hk s2h s1c s2c
# the family pruned to one leaf (the Append elided), probing the other relation's build: kept in place, out of the quals
permutation s2b s2q s1b s1u s2p s1c s2c
permutation s2b s1b s1m s2p s1c s2c
# the one leaf on the build side (the other relation's scan probing it): refused by name (FAMILY_JOIN)
permutation s2b s2t s1b s1u s2r s1c s2c
# SKIP LOCKED past a held row; the join under a Sort (ORDER BY)
permutation s2b s1b s1l s2s s1c s2c
permutation s2b s1b s1u s2o s1c s2c

# lockrows FAMLIMIT: a LIMIT / OFFSET above a row lock over a Sort of a partitioned / inheritance family's Append (ORDER BY
# .. LIMIT .. FOR UPDATE over the tree). ExecLockRows locks each row in the leaf its tableoid names and hands the
# EvalPlanQual row to the Limit, which counts only the rows the lock returns: a row whose newest version fails its quals
# is dropped and the next sorted row is taken; one updated in place is returned at its sorted place. Forms: an
# inheritance tree (the parent's own rows, a child with a column of its own), a partitioned tree whose second leaf is
# attached in another column order with a dropped column, OFFSET, SKIP LOCKED past a row another session holds, and a
# concurrent update moving a row out of the quals. Expected output is the C oracle's (PostgreSQL 18, pgdg).
setup
{
  CREATE TABLE gi (k int, v text);
  CREATE TABLE gi_c (extra int DEFAULT 7) INHERITS (gi);
  CREATE TABLE gi_d (w int) INHERITS (gi);
  INSERT INTO gi VALUES (1, 'a'), (11, 'a');
  INSERT INTO gi_c (k, v) VALUES (2, 'c'), (12, 'c');
  INSERT INTO gi_d (k, v) VALUES (3, 'd'), (13, 'd');
  CREATE TABLE gp (k int, v text, n int) PARTITION BY RANGE (k);
  CREATE TABLE gp1 PARTITION OF gp FOR VALUES FROM (0) TO (10);
  CREATE TABLE gp2 (n int, gone int, v text, k int);
  ALTER TABLE gp2 DROP COLUMN gone;
  ALTER TABLE gp ATTACH PARTITION gp2 FOR VALUES FROM (10) TO (20);
  INSERT INTO gp SELECT g, 'p', g * 10 FROM generate_series(1, 15) g;
  ANALYZE gi, gi_c, gi_d, gp;
}

teardown
{
  DROP TABLE gi CASCADE;
  DROP TABLE gp;
}

session s1
step s1b	{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1u	{ UPDATE gi SET v = v || '1' WHERE k = 2; }
step s1m	{ UPDATE gi SET k = 100 WHERE k = 2; }
step s1l	{ SELECT k FROM gi WHERE k = 2 FOR UPDATE; }
step s1pu	{ UPDATE gp SET v = 'q', n = -n WHERE k = 10; }
step s1pm	{ UPDATE gp SET n = 0 WHERE k = 9; }
step s1c	{ COMMIT; }

session s2
step s2b	{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s2x	{ EXPLAIN (COSTS OFF) SELECT tableoid::regclass, k, v FROM gi WHERE k IN (1, 2, 3, 12, 13) ORDER BY v DESC, k LIMIT 3 FOR UPDATE; }
step s2l	{ SELECT tableoid::regclass, k, v FROM gi WHERE k IN (1, 2, 3, 12, 13) ORDER BY v DESC, k LIMIT 3 FOR UPDATE; }
step s2o	{ SELECT tableoid::regclass, k, v FROM gi WHERE k < 50 ORDER BY v DESC, k OFFSET 1 LIMIT 2 FOR UPDATE; }
step s2s	{ SELECT tableoid::regclass, k, v FROM gi WHERE k < 50 ORDER BY v DESC, k LIMIT 3 FOR UPDATE SKIP LOCKED; }
step s2p	{ SELECT tableoid::regclass, k, v, n FROM gp WHERE n <> 0 AND k BETWEEN 9 AND 14 ORDER BY v DESC, k LIMIT 3 FOR UPDATE; }
step s2c	{ COMMIT; }

# an inheritance tree: the second sorted row's newest version kept in its place, then moved out of the quals (the next row taken)
permutation s2b s2x s1b s1u s2l s1c s2c
permutation s2b s1b s1m s2l s1c s2c
# OFFSET over the moved row
permutation s2b s1b s1m s2o s1c s2c
# SKIP LOCKED past a row the other session holds: the next row counted
permutation s2b s1b s1l s2s s1c s2c
# the partitioned tree (a reordered leaf with a dropped column): an update in place, then one failing the quals
permutation s2b s1b s1pu s2p s1c s2c
permutation s2b s1b s1pm s2p s1c s2c

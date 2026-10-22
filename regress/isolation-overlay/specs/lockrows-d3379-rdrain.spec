# lockrows D3379 RDRAIN (m7_epq_joins: `es LEFT JOIN eb .. FOR UPDATE OF es`): LockRows straight over a Hash Right Join,
# the locked relation rs hashed, rb's index scan probing (rb carries a REFERENCE mark, the nullable side). Gravity spools
# the probe's joined rows and its drain hands them to the lock, then rs's unmatched rows null-extended. s1 holds rs's row 2
# (k 2: four rb rows join it) or row 7 (k 7: no rb row, null-extended); after s1 commits, EvalPlanQual reruns the join
# over the newest version and the rb row the statement joined it with (its junk ctid, the version read), none for a
# null-extended row (its rb ctid NULL), never re-scanning rb:
#  s1v: the key kept, the value changed: the four joined rows carry the new value.
#  s1k8 / s1k3: the key moved to one no rb row has (8) or to another rb row's (3): the recheck's rb row no longer joins,
#  so each of the four rows comes back null-extended (never joined to the k 3 rows).
#  s1del: deleted, the four rows are dropped.
#  s1u7: row 7 moved to k 1: its rb ctid is NULL, so the recheck has no rb row and null-extends it again.
#  SKIP LOCKED passes over row 2 without waiting; NOWAIT raises on it.
# Expected output is PostgreSQL 18.6's (the C oracle).
setup
{
  CREATE TABLE rs (id int PRIMARY KEY, k int NOT NULL, v text NOT NULL) WITH (autovacuum_enabled = off);
  CREATE TABLE rb (id int PRIMARY KEY, k int NOT NULL) WITH (autovacuum_enabled = off);
  INSERT INTO rs SELECT g, g, 'init' FROM generate_series(1, 7) g;
  INSERT INTO rb SELECT g, g % 6 + 1 FROM generate_series(1, 5000) g;
  ANALYZE rs;
  ANALYZE rb;
}

teardown
{
  DROP TABLE rs, rb;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1v	{ UPDATE rs SET v = 'a' WHERE id = 2; }
step s1k8	{ UPDATE rs SET k = 8 WHERE id = 2; }
step s1k3	{ UPDATE rs SET k = 3 WHERE id = 2; }
step s1del	{ DELETE FROM rs WHERE id = 2; }
step s1u7	{ UPDATE rs SET k = 1, v = 'b' WHERE id = 7; }
step s1c	{ COMMIT; }

session s2
setup		{ SET enable_nestloop = off; SET enable_mergejoin = off; SET enable_memoize = off; }
step s2e	{ EXPLAIN (COSTS OFF) SELECT rs.id, rs.k, rs.v, rb.id FROM rs LEFT JOIN rb ON rb.k = rs.k AND rb.id < 20 FOR UPDATE OF rs; }
step s2l	{ SELECT rs.id, rs.k, rs.v, rb.id FROM rs LEFT JOIN rb ON rb.k = rs.k AND rb.id < 20 FOR UPDATE OF rs; }
step s2k	{ SELECT rs.id, rs.k, rs.v, rb.id FROM rs LEFT JOIN rb ON rb.k = rs.k AND rb.id < 20 FOR UPDATE OF rs SKIP LOCKED; }
step s2n	{ SELECT rs.id, rs.k, rs.v, rb.id FROM rs LEFT JOIN rb ON rb.k = rs.k AND rb.id < 20 FOR SHARE OF rs NOWAIT; }

permutation s2e s1c
permutation s1v s2l s1c
permutation s1k8 s2l s1c
permutation s1k3 s2l s1c
permutation s1del s2l s1c
permutation s1u7 s2l s1c
permutation s1k8 s2k s1c
permutation s1k8 s2n s1c

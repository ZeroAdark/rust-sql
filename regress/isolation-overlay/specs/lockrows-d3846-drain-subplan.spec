# D3846: a row lock over a Hash Right Join whose target list holds a SubPlan (e2e v4/m8_drain_spool_names :91's form).
# PostgreSQL plans LockRows over the Hash Right Join, db hashed (the locked, preserved side), dp probing with a REFERENCE
# mark, and computes the SubPlan over dc in the join's target list, below the lock. Over a newer version of a db row
# (READ COMMITTED) EvalPlanQual reruns the join over the test tuples (db's newest version, dp's row refetched at its junk
# ctid, none for a drained row's NULL ctid) and computes the SubPlan again over the row it returns, under the statement's
# snapshot. So: a v s1 changed gives the SubPlan's new row; a k s1 moved off the probe's key NULL-extends the joined rows;
# a drained row whose k s1 moved onto a probe key stays NULL-extended; a deleted row is skipped; a dc row s1 changed
# beside a db row it touched keeps the version the statement's snapshot sees. db 1 matches two dp rows (two locked rows,
# each rechecked), db 2 one, db 3 none (the drained row). s3 is the RIGHT SEMI drain (EXISTS) under the lock.
# Expected output is PostgreSQL 18.6's.
setup
{
  CREATE TABLE dp (id int PRIMARY KEY, k int NOT NULL);
  INSERT INTO dp VALUES (1, 10), (2, 10), (3, 20);
  INSERT INTO dp SELECT g, 1000 + g FROM generate_series(4, 2000) g;
  CREATE TABLE db (id int PRIMARY KEY, k int NOT NULL, v int NOT NULL);
  INSERT INTO db VALUES (1, 10, 1), (2, 20, 2), (3, 99, 3);
  CREATE TABLE dc (id int PRIMARY KEY, w text NOT NULL);
  INSERT INTO dc VALUES (1, 'c1'), (2, 'c2'), (3, 'c3'), (4, 'c4');
  ANALYZE;
}
teardown { DROP TABLE dp, db, dc; }

session s1
setup		{ BEGIN; }
step s1v	{ UPDATE db SET v = 4 WHERE id = 1; }
step s1k	{ UPDATE db SET k = 20 WHERE id = 1; }
step s1miss	{ UPDATE db SET k = 10 WHERE id = 3; }
step s1del	{ DELETE FROM db WHERE id = 2; }
step s1sub	{ UPDATE dc SET w = 'c1-new' WHERE id = 1; UPDATE db SET v = v WHERE id = 1; }
step s1c	{ COMMIT; }
teardown	{ ROLLBACK; }

session s2
setup		{ SET enable_mergejoin = off; SET enable_nestloop = off; }
step s2x	{ EXPLAIN (COSTS OFF) SELECT b.id, p.id AS pid, (SELECT c.w FROM dc c WHERE c.id = b.v) AS w FROM dp p RIGHT JOIN db b ON p.k = b.k FOR UPDATE OF b; }
step s2l	{ SELECT b.id, p.id AS pid, b.k, (SELECT c.w FROM dc c WHERE c.id = b.v) AS w FROM dp p RIGHT JOIN db b ON p.k = b.k FOR UPDATE OF b; }
step s2s	{ SELECT b.id, p.id AS pid, b.k, (SELECT c.w FROM dc c WHERE c.id = b.v) AS w FROM dp p RIGHT JOIN db b ON p.k = b.k FOR SHARE OF b; }

session s3
setup		{ SET enable_mergejoin = off; SET enable_nestloop = off; }
step s3x	{ EXPLAIN (COSTS OFF) SELECT b.id, (SELECT c.w FROM dc c WHERE c.id = b.v) AS w FROM db b WHERE EXISTS (SELECT 1 FROM dp p WHERE p.k = b.k) FOR UPDATE OF b; }
step s3l	{ SELECT b.id, b.k, (SELECT c.w FROM dc c WHERE c.id = b.v) AS w FROM db b WHERE EXISTS (SELECT 1 FROM dp p WHERE p.k = b.k) FOR UPDATE OF b; }

permutation s2x s1v s2l s1c
permutation s1k s2l s1c
permutation s1miss s2l s1c
permutation s1del s2l s1c
permutation s1sub s2l s1c
permutation s1v s2s s1c
permutation s3x s1v s3l s1c
permutation s1k s3l s1c
permutation s1del s3l s1c

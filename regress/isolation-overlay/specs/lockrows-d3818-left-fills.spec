# D3818: Hibernate's PESSIMISTIC_WRITE over an eager fetch, a row lock of the root over eight stacked LEFT joins (one
# per association), FOR UPDATE / NO KEY UPDATE OF lf0, every other relation REFERENCE. Eight NULL-filling joins or more
# lower the steps after them once for the matched and NULL-extended rows (Gx::fills_share), and the lock's EvalPlanQual
# redo branches at run time on the row each join held. Over a newer version of lf0 (READ COMMITTED) PostgreSQL reruns
# the joins over the test tuples: each REFERENCE row refetched at the row's junk ctid (the version the join read), none
# where the join NULL-extended. So a root whose fk s1 moved NULL-extends every join it matched (the refetched rows fail
# the join qual), and keeps NULL the ones it missed although rows for the new key exist; a root s1 only renamed keeps its
# joined rows; a REFERENCE row s1 changed keeps the version the join read; a deleted root is skipped. lf5 matches two
# rows (two locked rows, each rechecked), lf3 and lf6 none. s2 runs nested loops over seq scans (a Join Filter), s4
# nested loops over index probes (the nest's parameter, as Hibernate's plan over larger tables), s3 hash joins (lf0 the
# probe side, each other table hashed; lf5's two rows are one in its select list, so their order cannot show).
# Expected output is PostgreSQL 18.6's.
setup
{
  CREATE TABLE lf0 (id int PRIMARY KEY, fk int NOT NULL, v text NOT NULL);
  INSERT INTO lf0 VALUES (1, 10, 'a'), (2, 20, 'b');
  INSERT INTO lf0 SELECT g, 1000 + g, 'f' || g FROM generate_series(3, 60) g;
  CREATE TABLE lf1 (id int PRIMARY KEY, fk int NOT NULL, v text NOT NULL); CREATE INDEX lf1_fk ON lf1 (fk);
  INSERT INTO lf1 VALUES (101, 10, '1-10'), (102, 20, '1-20');
  CREATE TABLE lf2 (id int PRIMARY KEY, fk int NOT NULL, v text NOT NULL); CREATE INDEX lf2_fk ON lf2 (fk);
  INSERT INTO lf2 VALUES (201, 10, '2-10'), (202, 20, '2-20');
  CREATE TABLE lf3 (id int PRIMARY KEY, fk int NOT NULL, v text NOT NULL); CREATE INDEX lf3_fk ON lf3 (fk);
  INSERT INTO lf3 VALUES (301, 20, '3-20'), (302, 99, '3-99');
  CREATE TABLE lf4 (id int PRIMARY KEY, fk int NOT NULL, v text NOT NULL); CREATE INDEX lf4_fk ON lf4 (fk);
  INSERT INTO lf4 VALUES (401, 10, '4-10'), (402, 20, '4-20');
  CREATE TABLE lf5 (id int PRIMARY KEY, fk int NOT NULL, v text NOT NULL); CREATE INDEX lf5_fk ON lf5 (fk);
  INSERT INTO lf5 VALUES (501, 10, '5-10a'), (502, 10, '5-10b'), (503, 20, '5-20');
  CREATE TABLE lf6 (id int PRIMARY KEY, fk int NOT NULL, v text NOT NULL); CREATE INDEX lf6_fk ON lf6 (fk);
  INSERT INTO lf6 VALUES (601, 20, '6-20'), (602, 99, '6-99');
  CREATE TABLE lf7 (id int PRIMARY KEY, fk int NOT NULL, v text NOT NULL); CREATE INDEX lf7_fk ON lf7 (fk);
  INSERT INTO lf7 VALUES (701, 10, '7-10'), (702, 20, '7-20');
  CREATE TABLE lf8 (id int PRIMARY KEY, fk int NOT NULL, v text NOT NULL); CREATE INDEX lf8_fk ON lf8 (fk);
  INSERT INTO lf8 VALUES (801, 10, '8-10'), (802, 20, '8-20');
  ANALYZE;
}
teardown { DROP TABLE lf0, lf1, lf2, lf3, lf4, lf5, lf6, lf7, lf8; }

session s1
setup		{ BEGIN; }
step s1fk	{ UPDATE lf0 SET fk = 20 WHERE id = 1; }
step s1v	{ UPDATE lf0 SET v = 'a2' WHERE id = 1; }
step s1miss	{ UPDATE lf0 SET fk = 99 WHERE id = 1; }
step s1ref	{ UPDATE lf1 SET v = 'changed' WHERE fk = 10; UPDATE lf5 SET v = 'changed' WHERE id = 502; UPDATE lf0 SET v = 'a2' WHERE id = 1; }
step s1del	{ DELETE FROM lf0 WHERE id = 1; }
step s1c	{ COMMIT; }
teardown	{ ROLLBACK; }

session s2
setup		{ SET enable_hashjoin = off; SET enable_mergejoin = off; SET enable_material = off; SET enable_memoize = off; SET join_collapse_limit = 1; }
step s2x	{ EXPLAIN (COSTS OFF) SELECT lf0.id, lf0.fk, lf0.v, lf1.v, lf2.v, lf3.v, lf4.v, lf5.v, lf6.v, lf7.v, lf8.v FROM lf0 LEFT JOIN lf1 ON lf1.fk = lf0.fk LEFT JOIN lf2 ON lf2.fk = lf0.fk LEFT JOIN lf3 ON lf3.fk = lf0.fk LEFT JOIN lf4 ON lf4.fk = lf0.fk LEFT JOIN lf5 ON lf5.fk = lf0.fk LEFT JOIN lf6 ON lf6.fk = lf0.fk LEFT JOIN lf7 ON lf7.fk = lf0.fk LEFT JOIN lf8 ON lf8.fk = lf0.fk WHERE lf0.id = 1 FOR UPDATE OF lf0; }
step s2l	{ SELECT lf0.id, lf0.fk, lf0.v, lf1.v, lf2.v, lf3.v, lf4.v, lf5.v, lf6.v, lf7.v, lf8.v FROM lf0 LEFT JOIN lf1 ON lf1.fk = lf0.fk LEFT JOIN lf2 ON lf2.fk = lf0.fk LEFT JOIN lf3 ON lf3.fk = lf0.fk LEFT JOIN lf4 ON lf4.fk = lf0.fk LEFT JOIN lf5 ON lf5.fk = lf0.fk LEFT JOIN lf6 ON lf6.fk = lf0.fk LEFT JOIN lf7 ON lf7.fk = lf0.fk LEFT JOIN lf8 ON lf8.fk = lf0.fk WHERE lf0.id = 1 FOR UPDATE OF lf0; }
step s2k	{ SELECT lf0.id, lf0.fk, lf0.v, lf1.v, lf2.v, lf3.v, lf4.v, lf5.v, lf6.v, lf7.v, lf8.v FROM lf0 LEFT JOIN lf1 ON lf1.fk = lf0.fk LEFT JOIN lf2 ON lf2.fk = lf0.fk LEFT JOIN lf3 ON lf3.fk = lf0.fk LEFT JOIN lf4 ON lf4.fk = lf0.fk LEFT JOIN lf5 ON lf5.fk = lf0.fk LEFT JOIN lf6 ON lf6.fk = lf0.fk LEFT JOIN lf7 ON lf7.fk = lf0.fk LEFT JOIN lf8 ON lf8.fk = lf0.fk WHERE lf0.id = 1 FOR NO KEY UPDATE OF lf0; }

session s3
setup		{ SET enable_nestloop = off; SET enable_mergejoin = off; SET join_collapse_limit = 1; }
step s3x	{ EXPLAIN (COSTS OFF) SELECT lf0.id, lf0.fk, lf0.v, lf1.v, lf2.v, lf3.v, lf4.v, lf6.v, lf7.v, lf8.v FROM lf0 LEFT JOIN lf1 ON lf1.fk = lf0.fk LEFT JOIN lf2 ON lf2.fk = lf0.fk LEFT JOIN lf3 ON lf3.fk = lf0.fk LEFT JOIN lf4 ON lf4.fk = lf0.fk LEFT JOIN lf5 ON lf5.fk = lf0.fk LEFT JOIN lf6 ON lf6.fk = lf0.fk LEFT JOIN lf7 ON lf7.fk = lf0.fk LEFT JOIN lf8 ON lf8.fk = lf0.fk WHERE lf0.id + 0 <= 1 FOR UPDATE OF lf0; }
step s3l	{ SELECT lf0.id, lf0.fk, lf0.v, lf1.v, lf2.v, lf3.v, lf4.v, lf6.v, lf7.v, lf8.v FROM lf0 LEFT JOIN lf1 ON lf1.fk = lf0.fk LEFT JOIN lf2 ON lf2.fk = lf0.fk LEFT JOIN lf3 ON lf3.fk = lf0.fk LEFT JOIN lf4 ON lf4.fk = lf0.fk LEFT JOIN lf5 ON lf5.fk = lf0.fk LEFT JOIN lf6 ON lf6.fk = lf0.fk LEFT JOIN lf7 ON lf7.fk = lf0.fk LEFT JOIN lf8 ON lf8.fk = lf0.fk WHERE lf0.id + 0 <= 1 FOR UPDATE OF lf0; }

session s4
setup		{ SET enable_hashjoin = off; SET enable_mergejoin = off; SET enable_material = off; SET enable_memoize = off; SET enable_seqscan = off; SET join_collapse_limit = 1; }
step s4x	{ EXPLAIN (COSTS OFF) SELECT lf0.id, lf0.fk, lf0.v, lf1.v, lf2.v, lf3.v, lf4.v, lf5.v, lf6.v, lf7.v, lf8.v FROM lf0 LEFT JOIN lf1 ON lf1.fk = lf0.fk LEFT JOIN lf2 ON lf2.fk = lf0.fk LEFT JOIN lf3 ON lf3.fk = lf0.fk LEFT JOIN lf4 ON lf4.fk = lf0.fk LEFT JOIN lf5 ON lf5.fk = lf0.fk LEFT JOIN lf6 ON lf6.fk = lf0.fk LEFT JOIN lf7 ON lf7.fk = lf0.fk LEFT JOIN lf8 ON lf8.fk = lf0.fk WHERE lf0.id = 1 FOR UPDATE OF lf0; }
step s4l	{ SELECT lf0.id, lf0.fk, lf0.v, lf1.v, lf2.v, lf3.v, lf4.v, lf5.v, lf6.v, lf7.v, lf8.v FROM lf0 LEFT JOIN lf1 ON lf1.fk = lf0.fk LEFT JOIN lf2 ON lf2.fk = lf0.fk LEFT JOIN lf3 ON lf3.fk = lf0.fk LEFT JOIN lf4 ON lf4.fk = lf0.fk LEFT JOIN lf5 ON lf5.fk = lf0.fk LEFT JOIN lf6 ON lf6.fk = lf0.fk LEFT JOIN lf7 ON lf7.fk = lf0.fk LEFT JOIN lf8 ON lf8.fk = lf0.fk WHERE lf0.id = 1 FOR UPDATE OF lf0; }

permutation s2x s1fk s2l s1c
permutation s1v s2l s1c
permutation s1miss s2l s1c
permutation s1ref s2l s1c
permutation s1del s2l s1c
permutation s1fk s2k s1c
permutation s3x s1fk s3l s1c
permutation s1v s3l s1c
permutation s1miss s3l s1c
permutation s1ref s3l s1c
permutation s4x s1fk s4l s1c
permutation s1miss s4l s1c
permutation s1ref s4l s1c

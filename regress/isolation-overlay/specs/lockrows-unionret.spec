# lockrows UNION-RET (the twin of D2734's DELETE form) and NEST-CLAIM: an UPDATE of several result relations under
# RETURNING over the settled union of their scans meets target rows s1 changed. (1) The targets Append a nested loop's
# inner (an inheritance tree, a child in another column order): EvalPlanQual substitutes the row's result relation's
# scan with the newest version and blocks the others, so a version that still joins is written and returned over its
# new values, one that no longer qualifies or was deleted is skipped. (2) A data-modifying CTE through a partitioned
# root (a leaf in another column order; a raise-free SET list): the recheck in the IR, a row s1 moved raising 40001. (3) NEST-CLAIM: the Append
# of every leaf's primary key index scan the inner, pruned at run time by the outer row: the substituted leaf's newest
# version passes the arm's keys again (IndexRecheck): s1's new v is written and returned, a row whose key s1 changed is
# not written, a row s1 moved to another partition raises 40001. Rows come back in join order (no hash order).
# (4) D3379 UNION-OLD: NEST-CLAIM's UPDATE reading OLD's tableoid, the replaced version's: the row's own leaf (after a
# recheck the leaf of the version it locked), and for a row s2 itself moves (a new d past its leaf's range) the leaf it left.

setup
{
 CREATE TABLE ur1 (id int NOT NULL PRIMARY KEY, a int, junk1 text, b text);
 ALTER TABLE ur1 DROP COLUMN junk1;
 CREATE TABLE ur2 (c float) INHERITS (ur1);
 CREATE TABLE ur3 (id int NOT NULL PRIMARY KEY, c text, b text, a int);
 ALTER TABLE ur3 INHERIT ur1;
 INSERT INTO ur1 VALUES (1, 3, 'aaa'), (2, 4, 'bbb'), (3, 3, 'ccc');
 INSERT INTO ur2 VALUES (11, 3, 'abc', 1.1), (12, 4, 'bcd', 2.2);
 INSERT INTO ur3 VALUES (21, 'xxx', 'yyy', 3), (22, 'yyy', 'zzz', 4);
 INSERT INTO ur1 SELECT i, 100 + i, 'f' FROM generate_series(100, 300) i;
 INSERT INTO ur2 SELECT i, 100 + i, 'g', 0 FROM generate_series(400, 600) i;
 CREATE TABLE urp (id int NOT NULL, k int, v int, pad text) PARTITION BY RANGE (id);
 CREATE TABLE urp_1 PARTITION OF urp FOR VALUES FROM (1) TO (5);
 CREATE TABLE urp_2 (pad text, v int, k int, id int NOT NULL);
 ALTER TABLE urp ATTACH PARTITION urp_2 FOR VALUES FROM (5) TO (9);
 INSERT INTO urp SELECT i, i % 2, i * 10, 'p' || i FROM generate_series(1, 8) i;
 CREATE TABLE unc (d date NOT NULL, id int NOT NULL, v int, PRIMARY KEY (d, id)) PARTITION BY RANGE (d);
 CREATE TABLE unc_a PARTITION OF unc FOR VALUES FROM ('2024-01-01') TO ('2024-03-01');
 CREATE TABLE unc_b (v int, id int NOT NULL, d date NOT NULL);
 ALTER TABLE unc ATTACH PARTITION unc_b FOR VALUES FROM ('2024-03-01') TO ('2024-06-01');
 CREATE TABLE unc_d PARTITION OF unc DEFAULT;
 INSERT INTO unc SELECT date '2024-01-01' + (i % 200), i, i FROM generate_series(1, 3000) i;
 CREATE TABLE usrc (d date, id int);
 INSERT INTO usrc SELECT date '2024-01-01' + (i % 200), i FROM generate_series(10, 3000, 370) i;
}
setup { ANALYZE ur1, ur2, ur3, urp, unc, usrc; }

teardown
{
 DROP TABLE ur1 CASCADE;
 DROP TABLE urp;
 DROP TABLE unc;
 DROP TABLE usrc;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1r	{ UPDATE ur3 SET b = 'Y' WHERE id = 21; }
step s1q	{ DELETE FROM ur1 WHERE id = 3; }
step s1a	{ UPDATE ur1 SET a = 5 WHERE id = 1; }
step s1v	{ UPDATE urp SET v = v + 1000 WHERE id IN (2, 6); }
step s1m	{ UPDATE urp SET id = 7 WHERE id = 4; }
step s1k	{ UPDATE urp SET k = 1 WHERE id IN (4, 8); }
step s1nv	{ UPDATE unc SET v = v + 1000 WHERE id IN (380, 750); }
step s1nk	{ UPDATE unc SET id = id + 10000 WHERE id = 1120; }
step s1nm	{ UPDATE unc SET d = d + 100 WHERE id = 750; }
step s1c	{ COMMIT; }
step s1b	{ ROLLBACK; }

session s2
setup		{ SET max_parallel_workers_per_gather = 0; SET enable_hashjoin = off; SET enable_mergejoin = off; BEGIN ISOLATION LEVEL READ COMMITTED; }
step s2r	{ UPDATE ur1 SET b = ur1.b || '+' || ur2.id FROM ur2 WHERE ur1.a = 3 AND ur2.a = 3 RETURNING ur1.id, ur1.b, old.b AS ob, ur1.tableoid::regclass AS rel; }
step s2d	{ WITH u AS (UPDATE urp SET k = k WHERE k = 0 RETURNING id, v, pad, tableoid::regclass AS leaf) SELECT * FROM u ORDER BY id; }
step s2n	{ UPDATE unc SET v = unc.v + 1 FROM usrc s WHERE unc.d = s.d AND unc.id = s.id RETURNING unc.id, unc.v, unc.tableoid::regclass AS leaf; }
step s2o	{ UPDATE unc SET v = unc.v + 1 FROM usrc s WHERE unc.d = s.d AND unc.id = s.id RETURNING unc.id, unc.v, old.v AS ov, old.tableoid::regclass AS oleaf, unc.tableoid::regclass AS leaf; }
step s2m	{ UPDATE unc SET d = unc.d + 60 FROM usrc s WHERE unc.d = s.d AND unc.id = s.id RETURNING unc.id, old.d AS od, unc.d, old.v AS ov, unc.v, old.tableoid::regclass AS oleaf, unc.tableoid::regclass AS leaf; }
step s2c	{ COMMIT; }

session s3
setup		{ SET max_parallel_workers_per_gather = 0; SET enable_hashjoin = off; SET enable_mergejoin = off; }
step s3er	{ EXPLAIN (COSTS OFF) UPDATE ur1 SET b = ur1.b || '+' || ur2.id FROM ur2 WHERE ur1.a = 3 AND ur2.a = 3 RETURNING ur1.id; }
step s3en	{ EXPLAIN (COSTS OFF) UPDATE unc SET v = unc.v + 1 FROM usrc s WHERE unc.d = s.d AND unc.id = s.id RETURNING unc.id; }
step s3r	{ SELECT tableoid::regclass, id, a, b FROM ur1 WHERE id < 100 ORDER BY 1, 2; }
step s3p	{ SELECT tableoid::regclass, id, k, v, pad FROM urp ORDER BY id; }
step s3n	{ SELECT tableoid::regclass, id, v FROM unc WHERE id % 370 = 10 OR id > 10000 ORDER BY id; }

permutation s3er s1r s1q s1a s2r s1c s2c s3r
permutation s1r s2r s1b s2c s3r
permutation s1v s1k s2d s1c s2c s3p
permutation s1m s2d s1c s2c s3p
permutation s3en s1nv s1nk s2n s1c s2c s3n
permutation s1nm s2n s1c s2c s3n
permutation s1nv s2n s1b s2c s3n
permutation s1nv s2o s1c s2c s3n
permutation s1nv s2m s1c s2c s3n

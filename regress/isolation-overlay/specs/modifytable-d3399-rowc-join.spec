# D3399 ROWC-JOIN (modifytable, D3379): a data-modifying CTE's UPDATE ... FROM over an INNER hash probe whose build side
# is itself a heap scan probing another relation (o's probe of the Hash over c's Hash Join with r, both REFERENCE-marked)
# rechecks in the IR, the newest version matched against the joined row the probe handed on. s2's CTE waits on the order
# rows s1 changed; after s1 commits, the recheck writes the newest version of a row s1 changed in another column, skips
# one s1 moved out of the qual, skips one s1 moved to another customer (EvalPlanQual joins the newest version to the
# customer and region rows the join read, refetched at their junk ctids), and keeps a row whose region s1 closed no
# longer: the region row is the version the join read. With s1 rolled back, s2 writes the old versions.

setup
{
 CREATE TABLE jo (id int PRIMARY KEY, cid int NOT NULL, rid int NOT NULL, status text NOT NULL, total int NOT NULL);
 CREATE TABLE jc (id int PRIMARY KEY, active bool NOT NULL, region int NOT NULL);
 CREATE TABLE jr (id int PRIMARY KEY, closed bool NOT NULL);
 INSERT INTO jr SELECT g, g % 3 = 0 FROM generate_series(0, 9) g;
 INSERT INTO jc SELECT g, g % 4 <> 0, g % 10 FROM generate_series(1, 300) g;
 INSERT INTO jo SELECT g, g % 300 + 1, g % 10, CASE WHEN g % 3 = 0 THEN 'pending' WHEN g % 3 = 1 THEN 'done' ELSE 'cancelled' END, g % 50 FROM generate_series(1, 3000) g;
 ANALYZE jo, jc, jr;
}

teardown
{
 DROP TABLE jo, jc, jr;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1amt	{ UPDATE jo SET total = total + 1 WHERE id = 19; }
step s1st	{ UPDATE jo SET status = 'pending' WHERE id = 22; }
step s1cid	{ UPDATE jo SET cid = 2 WHERE id = 25; }
step s1amt2	{ UPDATE jo SET total = total + 1 WHERE id = 28; }
step s1reg	{ UPDATE jr SET closed = false WHERE id = 9; }
step s1c	{ COMMIT; }
step s1r	{ ROLLBACK; }

session s2
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; SET enable_nestloop = off; SET enable_mergejoin = off; }
step s2x	{ EXPLAIN (COSTS OFF) WITH upd AS (UPDATE jo o SET total = o.total + 100 FROM jc c, jr r WHERE c.id = o.cid AND r.id = c.region AND r.closed AND o.status = 'done' RETURNING o.id, o.total) SELECT count(*), sum(total) FROM upd; }
step s2u	{ WITH upd AS (UPDATE jo o SET total = o.total + 100 FROM jc c, jr r WHERE c.id = o.cid AND r.id = c.region AND r.closed AND o.status = 'done' RETURNING o.id, o.total) SELECT count(*), sum(total) FROM upd; }
step s2c	{ COMMIT; }

session s3
step s3sel	{ SELECT id, cid, status, total FROM jo WHERE id IN (19, 22, 25, 28, 49) ORDER BY id; }
step s3cnt	{ SELECT status, count(*), sum(total) FROM jo GROUP BY status ORDER BY status; }

permutation s2x s1amt s1st s1cid s1amt2 s1reg s2u s1c s2c s3sel s3cnt
permutation s1amt s1st s2u s1r s2c s3sel s3cnt

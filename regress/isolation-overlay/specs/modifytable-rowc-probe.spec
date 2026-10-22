# rowc-probe (modifytable, D655 Row C): a data-modifying CTE's UPDATE ... FROM / DELETE ... USING over an INNER hash probe
# of a REFERENCE-marked relation rechecks in the IR. s2's CTE waits on the order rows s1 changed; after s1 commits, the
# recheck writes the newest version of a row s1 changed in another column, skips one s1 moved out of the qual, and skips
# one s1 moved to another customer (EvalPlanQual joins the newest version to the customer row the join read, refetched at
# its junk ctid, never to the new one); a customer s1 changed keeps the version the join read. With s1 rolled back, s2
# writes the old versions.

setup
{
 CREATE TABLE rpo (id int PRIMARY KEY, cid int NOT NULL, status text NOT NULL, amt int NOT NULL);
 CREATE TABLE rpc (id int PRIMARY KEY, active bool NOT NULL);
 INSERT INTO rpc SELECT g, g % 4 <> 0 FROM generate_series(1, 100) g;
 INSERT INTO rpo SELECT g, g % 100 + 1, CASE WHEN g % 3 = 0 THEN 'pending' ELSE 'done' END, g FROM generate_series(1, 1000) g;
 ANALYZE rpo, rpc;
}

teardown
{
 DROP TABLE rpo, rpc;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1amt	{ UPDATE rpo SET amt = amt + 1 WHERE id = 3; }
step s1st	{ UPDATE rpo SET status = 'done' WHERE id = 207; }
step s1cid	{ UPDATE rpo SET cid = 16 WHERE id = 111; }
step s1cust	{ UPDATE rpc SET active = true WHERE id = 4; }
step s1c	{ COMMIT; }
step s1r	{ ROLLBACK; }

session s2
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; SET LOCAL enable_nestloop = off; SET LOCAL enable_mergejoin = off; }
step s2u	{ WITH upd AS (UPDATE rpo o SET status = 'on_hold' FROM rpc c WHERE c.id = o.cid AND NOT c.active AND o.status = 'pending' RETURNING o.id) SELECT count(*) FROM upd; }
step s2d	{ WITH gone AS (DELETE FROM rpo o USING rpc c WHERE c.id = o.cid AND NOT c.active AND o.status = 'pending') SELECT count(*) FROM rpo; }
step s2c	{ COMMIT; }

session s3
step s3sel	{ SELECT id, cid, status, amt FROM rpo WHERE id IN (3, 111, 207, 303, 411) ORDER BY id; }
step s3cnt	{ SELECT status, count(*) FROM rpo GROUP BY status ORDER BY status; }

permutation s1amt s1st s1cid s2u s1c s2c s3sel s3cnt
permutation s1amt s1st s1cid s2d s1c s2c s3sel s3cnt
permutation s1amt s1cust s2u s1c s2c s3sel s3cnt
permutation s1amt s1st s2u s1r s2c s3sel s3cnt

# rowc-copy (modifytable, D655 Row C): a data-modifying CTE's DELETE semi-joined to another writing CTE's rows (a Hash
# Semi Join over the CTE Scan, a COPY mark) rechecks in the IR. s2's DELETE waits on the side rows s1 changed; after s1
# commits, the recheck deletes the newest version of a row s1 changed in another column, skips the row s1 moved to
# another id (EvalPlanQual semi-joins the newest version to the CTE row the join read, never to another), and skips the
# row s1 deleted. With s1 rolled back, s2 deletes the old versions.

setup
{
 CREATE TABLE rcb (id int PRIMARY KEY, grp int NOT NULL, amt int NOT NULL);
 CREATE TABLE rcs (id int PRIMARY KEY, v int NOT NULL);
 INSERT INTO rcb SELECT g, g % 10, g FROM generate_series(1, 500) g;
 INSERT INTO rcs SELECT g, g FROM generate_series(1, 2000) g;
 ANALYZE rcb, rcs;
}

teardown
{
 DROP TABLE rcb, rcs;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1v	{ UPDATE rcs SET v = v + 1000 WHERE id = 7; }
step s1id	{ UPDATE rcs SET id = 2717 WHERE id = 17; }
step s1del	{ DELETE FROM rcs WHERE id = 27; }
step s1c	{ COMMIT; }
step s1r	{ ROLLBACK; }

session s2
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; SET LOCAL enable_nestloop = off; SET LOCAL enable_mergejoin = off; }
step s2d	{ WITH u AS (UPDATE rcb SET amt = amt + 1 WHERE grp = 7 RETURNING id), d AS (DELETE FROM rcs WHERE id IN (SELECT id FROM u) RETURNING id, v) SELECT (SELECT count(*) FROM u), (SELECT count(*) FROM d), (SELECT sum(v) FROM d); }
step s2c	{ COMMIT; }

session s3
step s3sel	{ SELECT id, v FROM rcs WHERE id IN (7, 17, 27, 37, 2717) ORDER BY id; }
step s3cnt	{ SELECT count(*), sum(v) FROM rcs; }

permutation s1v s1id s1del s2d s1c s2c s3sel s3cnt
permutation s1v s1id s1del s2d s1r s2c s3sel s3cnt

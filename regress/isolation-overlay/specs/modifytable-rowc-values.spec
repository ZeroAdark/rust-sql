# rowc-values (modifytable, D655 Row C): a data-modifying CTE's UPDATE ... FROM a VALUES list (a Hash Join over the
# Values Scan, a COPY mark) rechecks in the IR. s2's UPDATE waits on the rows s1 changed; after s1 commits, the recheck
# updates the newest version of a row s1 changed in another column, skips the row s1 moved to another id (EvalPlanQual
# joins the newest version to the VALUES row the join read, never to another), and skips the row s1 deleted. With s1
# rolled back, s2 updates the old versions.

setup
{
 CREATE TABLE rvs (id int PRIMARY KEY, v int NOT NULL);
 INSERT INTO rvs SELECT g, g FROM generate_series(1, 2000) g;
 ANALYZE rvs;
}

teardown
{
 DROP TABLE rvs;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1v	{ UPDATE rvs SET v = v + 1000 WHERE id = 7; }
step s1id	{ UPDATE rvs SET id = 2717 WHERE id = 17; }
step s1del	{ DELETE FROM rvs WHERE id = 27; }
step s1c	{ COMMIT; }
step s1r	{ ROLLBACK; }

session s2
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; SET LOCAL enable_nestloop = off; SET LOCAL enable_mergejoin = off; }
step s2u	{ WITH w AS (UPDATE rvs t SET v = t.v + x.d FROM (VALUES (7, 1), (17, 2), (27, 3), (37, 4)) AS x(id, d) WHERE x.id = t.id RETURNING t.id, t.v) SELECT count(*), sum(v) FROM w; }
step s2c	{ COMMIT; }

session s3
step s3sel	{ SELECT id, v FROM rvs WHERE id IN (7, 17, 27, 37, 2717) ORDER BY id; }

permutation s1v s1id s1del s2u s1c s2c s3sel
permutation s1v s1id s1del s2u s1r s2c s3sel

# famnest (modifytable, rowsecurity#338 #339's form): an UPDATE / DELETE whose target's own scan is the outer of a nested
# loop whose inner is the Append of a REFERENCE-marked inheritance parent's children (decompose settles it into a union
# the nest's inner reads) meets a target row s1 changed. EvalPlanQual takes the target's newest version and refetches the
# joined child's row at its junk ctid (the version the statement read, s1's change to it unseen), only in the child the
# row's tableoid names (the other children give no row), its quals checked again over that version, and reruns the join:
# the parent's own row (arm 1), a child's (arm 2) and the other child's (arm 3, its columns in another order) each join
# their target row again, a target row s1 moved out of the qual or deleted is skipped, and one whose join key s1 moved
# onto another child's row (id 101 -> 113, joining fnc1's 13) is skipped too: the recheck joins the one refetched row the
# statement read (fnp's 1), never the inner's other rows. With s1 rolled back, s2 writes the old versions. The tables stay unanalyzed, so PostgreSQL plans the target's scan (estimated one row) as the outer and
# the Append, under no Materialize, as the inner, as in rowsecurity#339.

setup
{
 CREATE TABLE fnp (id int NOT NULL, a int, b text);
 CREATE TABLE fnc1 (c float) INHERITS (fnp);
 CREATE TABLE fnc2 (id int NOT NULL, c text, b text, a int);
 ALTER TABLE fnc2 INHERIT fnp;
 CREATE TABLE fnt (id int NOT NULL, a int, b text);
 INSERT INTO fnp VALUES (1, 3, 'p1'), (3, 4, 'p3'), (5, 4, 'p5'), (7, 4, 'p7');
 INSERT INTO fnc1 VALUES (11, 3, 'c11', 1.1), (13, 3, 'c13', 1.3), (15, 4, 'c15', 1.5);
 INSERT INTO fnc1 SELECT 1009 + 10 * g, 3, 'pad', 0 FROM generate_series(1, 300) g;
 INSERT INTO fnc2 VALUES (21, 'x', 'c21', 4), (25, 'y', 'c25', 3);
 INSERT INTO fnt VALUES (101, 3, 't101'), (103, 3, 't103'), (105, 3, 't105'), (107, 3, 't107'), (102, 4, 't102');
}

teardown
{
 DROP TABLE fnp CASCADE;
 DROP TABLE fnt;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1t	{ UPDATE fnt SET b = b || '*' WHERE id IN (101, 103, 105); }
step s1p	{ UPDATE fnp SET a = 4, b = b || '#' WHERE id IN (1, 13, 25); }
step s1q	{ UPDATE fnt SET a = 4 WHERE id = 103; }
step s1d	{ DELETE FROM fnt WHERE id = 105; }
step s1k	{ UPDATE fnt SET id = 113 WHERE id = 101; }
step s1c	{ COMMIT; }
step s1r	{ ROLLBACK; }

session s2
setup		{ SET max_parallel_workers_per_gather = 0; BEGIN ISOLATION LEVEL READ COMMITTED; }
step s2u	{ UPDATE fnt SET b = fnt.b || '+' || fnp.b FROM fnp WHERE fnt.a = 3 AND fnt.id > 100 AND fnt.b > 's' AND fnp.a = 3 AND fnp.id % 10 = fnt.id % 10; }
step s2d	{ DELETE FROM fnt USING fnp WHERE fnt.a = 3 AND fnt.id > 100 AND fnt.b > 's' AND fnp.a = 3 AND fnp.id % 10 = fnt.id % 10; }
step s2c	{ COMMIT; }

session s3
setup		{ SET max_parallel_workers_per_gather = 0; }
step s3sel	{ SELECT id, a, b FROM fnt ORDER BY id; }
step s3eu	{ EXPLAIN (COSTS OFF) UPDATE fnt SET b = fnt.b || '+' || fnp.b FROM fnp WHERE fnt.a = 3 AND fnt.id > 100 AND fnt.b > 's' AND fnp.a = 3 AND fnp.id % 10 = fnt.id % 10; }
step s3ed	{ EXPLAIN (COSTS OFF) DELETE FROM fnt USING fnp WHERE fnt.a = 3 AND fnt.id > 100 AND fnt.b > 's' AND fnp.a = 3 AND fnp.id % 10 = fnt.id % 10; }

permutation s3eu s1t s1p s2u s1c s2c s3sel
permutation s1t s1q s1d s2u s1c s2c s3sel
permutation s3ed s1t s1p s2d s1c s2c s3sel
permutation s1t s1q s2d s1c s2c s3sel
permutation s1t s1p s2u s1r s2c s3sel
permutation s1k s2u s1c s2c s3sel
permutation s1k s2d s1c s2c s3sel

# D3278 (DRAINED): the statement's own UPDATE / DELETE beside a data-modifying CTE nothing reads whose RETURNING rows
# go to a store (INSERT, UPDATE and DELETE CTEs; nextval in the RETURNING list counts the CTE's rows). PostgreSQL runs
# such a CTE's ModifyTable once, at ExecPostprocessPlan, after the statement's own; when the own write meets a row a
# concurrent transaction updated, EvalPlanQual rechecks it over the newest version and never runs the CTE again.
# Permutations: a concurrent update then commit, then abort; a key move out of the statement's range; a delete.
setup
{
  CREATE TABLE dt (id int PRIMARY KEY, v int NOT NULL);
  INSERT INTO dt SELECT g, g FROM generate_series(1, 4) g;
  CREATE TABLE dl (id int, note text);
  INSERT INTO dl SELECT g, 'n' || g FROM generate_series(1, 4) g;
  CREATE SEQUENCE dsq;
  ANALYZE dt, dl;
}
teardown
{
  DROP TABLE dt, dl;
  DROP SEQUENCE dsq;
}
session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1u	{ UPDATE dt SET v = v + 100 WHERE id = 1; }
step s1k	{ UPDATE dt SET id = 10 WHERE id = 1; }
step s1d	{ DELETE FROM dt WHERE id = 1; }
step s1c	{ COMMIT; }
step s1a	{ ABORT; }
session s2
step s2d	{ WITH d AS (DELETE FROM dl WHERE id <= 2 RETURNING id, nextval('dsq') AS n) UPDATE dt SET v = v + 1 WHERE id <= 2; }
step s2i	{ WITH i AS (INSERT INTO dl VALUES (9, 'x') RETURNING id, nextval('dsq') AS n) DELETE FROM dt WHERE id = 1; }
step s2u	{ WITH u AS (UPDATE dl SET note = note || 'q' WHERE id >= 3 RETURNING id, nextval('dsq') AS n) UPDATE dt SET v = v * 10 WHERE v >= 1; }
session s3
step s3sel	{ SELECT 'dt' AS t, id, v::text AS x FROM dt UNION ALL SELECT 'dl', id, note FROM dl ORDER BY 1, 2, 3; }
step s3seq	{ SELECT last_value, is_called FROM dsq; }
permutation s1u s2d s1c s3sel s3seq
permutation s1u s2d s1a s3sel s3seq
permutation s1k s2d s1c s3sel s3seq
permutation s1d s2d s1c s3sel s3seq
permutation s1u s2i s1c s3sel s3seq
permutation s1k s2i s1c s3sel s3seq
permutation s1u s2u s1c s3sel s3seq
permutation s1d s2u s1c s3sel s3seq

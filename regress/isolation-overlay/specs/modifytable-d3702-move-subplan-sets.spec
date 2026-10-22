# D3702 (material_update_view_arms :98 / :99): an UPDATE that moves rows between range partitions (its SET changes the
# partition key) whose arms carry per-row SubPlan steps with rerun sets of their own: the view's qual and its check option
# call an aggregate SubPlan correlated to the row. Over a newer version of a row (READ COMMITTED) PostgreSQL's EvalPlanQual
# re-runs the arm's quals, the SubPlan included, on the newest version: a row s1 moved out of the qual is skipped, a row
# s1 deleted is skipped, and the check option then runs over the row the recheck made (its outcome follows s1's value).
# The SubPlan's own scan runs under the statement's snapshot: s1's committed mtab row stays unseen. A row s1 moved to
# another partition first raises 40001. Expected output is PostgreSQL 18.6's.
setup
{
  CREATE TABLE mv (id int NOT NULL, k int, v int, c numeric) PARTITION BY RANGE (k);
  CREATE TABLE mv0 PARTITION OF mv FOR VALUES FROM (0) TO (10);
  CREATE TABLE mv1 PARTITION OF mv FOR VALUES FROM (10) TO (20);
  CREATE TABLE mv2 PARTITION OF mv FOR VALUES FROM (20) TO (40);
  INSERT INTO mv VALUES (1, 1, 10, 150), (2, 2, 20, 160), (3, 11, 30, 170), (4, 12, 40, 180), (5, 21, 50, 190), (6, 22, 60, 200);
  CREATE TABLE mtab (c1 int);
  INSERT INTO mtab VALUES (60), (100);
  CREATE VIEW mvv AS SELECT * FROM mv WHERE (SELECT max(c1) + 0 * c FROM mtab) < c WITH CHECK OPTION;
}
teardown { DROP VIEW mvv; DROP TABLE mv, mtab; }

session s1
setup		{ BEGIN; }
step s1u	{ UPDATE mv SET c = c - 60, v = v + 1 WHERE id IN (1, 3); }
step s1c10	{ UPDATE mv SET c = c + 10 WHERE id = 1; }
step s1d	{ DELETE FROM mv WHERE id = 3; }
step s1k	{ UPDATE mv SET k = k + 1 WHERE id IN (1, 3); }
step s1x	{ UPDATE mv SET k = 15 WHERE id = 1; }
step s1t	{ INSERT INTO mtab VALUES (165); }
step s1c	{ COMMIT; }

session s2
setup		{ SET max_parallel_workers_per_gather = 0; }
step s2m	{ UPDATE mvv SET k = k + 10 WHERE id IN (1, 2, 3, 4); }
step s2mc	{ UPDATE mvv SET k = k + 10, c = c - 55 WHERE id IN (1, 2); }
step s2w	{ UPDATE mv SET k = k + 10 WHERE (SELECT max(c1) + 0 * c FROM mtab) < c AND id IN (1, 3); }
step s2r	{ SELECT tableoid::regclass AS part, * FROM mv ORDER BY id; }

permutation s1u s2m s1c s2r
permutation s1c10 s2mc s1c s2r
permutation s1d s2m s1c s2r
permutation s1k s2m s1c s2r
permutation s1u s2w s1c s2r
permutation s1x s2m s1c s2r
permutation s1c10 s1t s2m s1c s2r

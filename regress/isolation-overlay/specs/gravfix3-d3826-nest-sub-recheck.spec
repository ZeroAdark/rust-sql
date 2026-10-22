# D3826 (1): Hibernate ORM 7's BulkManipulationTest DELETE of a many-to-many join table (`delete from MANY_TO_MANY
# to_delete_ where to_delete_.IN_ID in (select sewa1_0.ID from SIMPLE_1 sewa1_0 where (select count(*) from SIMPLE_2
# ae1_0 where sewa1_0.ID=ae1_0.SIMPLE_1_ID)=0 and sewa1_0.NAME like '%' escape '')`). Another session changes a target
# row first (READ COMMITTED), so s2's write waits on it, then EvalPlanQual rechecks the newest version: SIMPLE_1's row
# refetched (its REFERENCE mark), the count's row as the statement read it (pgrust's decorrelated subquery, a COPY mark:
# its stored row, or none when null-extended; PostgreSQL 18.6's SubPlan runs again under the statement's snapshot, the
# same count), then the inner's recheck condition over the newest version.
#  (a) s1 changes the row's OUT_ID: s2 deletes the newest version
#  (b) s1 moves the row's IN_ID to another SIMPLE_1 row: s2 skips it (the inner's condition fails over the new version)
#  (c) s1 also adds a SIMPLE_2 row for the joined SIMPLE_1 row: the rerun SubPlan reads the statement's snapshot, which
#      does not see it, so s2 still deletes the newest version
#  (d) s1 deletes the row: s2 skips it
#  (e) s1 aborts (control)
# Each prints the table after s2's write. The expected output is PostgreSQL 18.6's.
setup
{
  CREATE TABLE d3826s1 (id bigint PRIMARY KEY, name varchar(255));
  CREATE TABLE d3826s2 (id bigint PRIMARY KEY, name varchar(255), simple_1_id bigint REFERENCES d3826s1);
  CREATE TABLE d3826mm (in_id bigint NOT NULL REFERENCES d3826s1, out_id bigint NOT NULL REFERENCES d3826s1, PRIMARY KEY (in_id, out_id));
  INSERT INTO d3826s1 VALUES (1, 'a'), (2, 'b'), (3, 'c'), (4, 'd');
  INSERT INTO d3826s2 VALUES (10, 'x', 1);
  INSERT INTO d3826mm VALUES (1, 2), (2, 3), (3, 1), (4, 1);
}
teardown { DROP TABLE d3826mm, d3826s2, d3826s1; }

session s1
setup		{ BEGIN; }
step s1out	{ UPDATE d3826mm SET out_id = 4 WHERE in_id = 2; }
step s1in	{ UPDATE d3826mm SET in_id = 1 WHERE in_id = 2; }
step s1sub	{ INSERT INTO d3826s2 VALUES (20, 'y', 2); UPDATE d3826mm SET out_id = 4 WHERE in_id = 2; }
step s1del	{ DELETE FROM d3826mm WHERE in_id = 2; }
step s1c	{ COMMIT; }
step s1a	{ ROLLBACK; }

session s2
setup		{ BEGIN; }
step s2del	{ delete from d3826mm to_delete_ where to_delete_.in_id in (select sewa1_0.id from d3826s1 sewa1_0 where (select count(*) from d3826s2 ae1_0 where sewa1_0.id=ae1_0.simple_1_id)=0 and sewa1_0.name like '%' escape '') }
step s2show	{ SELECT * FROM d3826mm ORDER BY 1, 2; }
step s2c	{ COMMIT; }

permutation s1out s2del s1c s2show s2c
permutation s1in s2del s1c s2show s2c
permutation s1sub s2del s1c s2show s2c
permutation s1del s2del s1c s2show s2c
permutation s1out s2del s1a s2show s2c

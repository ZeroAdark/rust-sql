# D3838 ROWC-INIT: Hibernate ORM 7's CTE mutation UPDATE with a subquery in SET (`with id_cte (id) as materialized
# (..), update_cte_Animal (id) as (update Animal a1_0 set mother_id=(select a2_0.id from Animal a2_0 where a2_0.id=1)
# where a1_0.id in (select id.id from id_cte id) returning id) select count(*) from id_cte id`): PostgreSQL 18.6 attaches
# the subquery to the CTE's ModifyTable as an InitPlan, the write over a Nested Loop (a HashAggregate of the CTE Scan,
# the target's index scan). Another session changes a target row first (READ COMMITTED), so s2's write waits on it, then
# EvalPlanQual rechecks the newest version. EvalPlanQualStart initializes the CTE's ModifyTable afresh, arming its
# InitPlans, so the first recheck runs the InitPlan again: a nextval in SET is called twice, the rechecked row taking the
# second value (a later recheck would read the statement's own value).
#  (a) s1 changes the row's weight: s2's UPDATE writes the newest version, its serial from the second nextval
#  (b) the same over two ids, only one row changed by s1: the other row takes the first value
#  (c) Hibernate's own SET (a constant InitPlan): its value again
#  (d) s1 deletes the row: no recheck, one nextval
#  (e) s1 aborts (control): one nextval
# Each prints the table and the sequence's last value after s2's write. The expected output is PostgreSQL 18.6's.
setup
{
  CREATE TABLE d3838a (id bigint PRIMARY KEY, body_weight float4 NOT NULL, mother_id bigint, serial text);
  INSERT INTO d3838a VALUES (1, 10, NULL, 'a1'), (2, 20, NULL, 'a2'), (3, 30, NULL, 'a3'), (4, 40, NULL, 'a4');
  CREATE SEQUENCE d3838s;
}
teardown { DROP TABLE d3838a; DROP SEQUENCE d3838s; }

session s1
setup		{ BEGIN; }
step s1upd	{ UPDATE d3838a SET body_weight = 99 WHERE id = 2; }
step s1del	{ DELETE FROM d3838a WHERE id = 2; }
step s1c	{ COMMIT; }
step s1a	{ ROLLBACK; }

session s2
setup		{ BEGIN; }
step s2x	{ EXPLAIN (COSTS OFF) with id_cte (id) as materialized (select a1_0.id from d3838a a1_0 where a1_0.id = 2), u (id) as (update d3838a a set serial = 's' || (select nextval('d3838s')) where a.id in (select id.id from id_cte id) returning id) select count(*) from id_cte id; }
step s2nv	{ with id_cte (id) as materialized (select a1_0.id from d3838a a1_0 where a1_0.id = 2), u as (update d3838a a set serial = 's' || (select nextval('d3838s')) where a.id in (select id.id from id_cte id) returning a.id, a.body_weight, a.serial) select * from u; }
step s2nv2	{ with id_cte (id) as materialized (select a1_0.id from d3838a a1_0 where a1_0.id in (2, 3)), u as (update d3838a a set serial = 's' || (select nextval('d3838s')) where a.id in (select id.id from id_cte id) returning a.id, a.body_weight, a.serial) select * from u order by id; }
step s2mom	{ with id_cte (id) as materialized (select a1_0.id from d3838a a1_0 where a1_0.id = 2), update_cte_a (id) as (update d3838a a1_0 set mother_id = (select a2_0.id from d3838a a2_0 where a2_0.id = 1) where a1_0.id in (select id.id from id_cte id) returning id) select count(*) from id_cte id; }
step s2show	{ SELECT a.*, (SELECT last_value FROM d3838s) AS seq FROM d3838a a ORDER BY id; }
step s2c	{ COMMIT; }

permutation s2x s1upd s2nv s1c s2show s2c
permutation s1upd s2nv2 s1c s2show s2c
permutation s1upd s2mom s1c s2show s2c
permutation s1del s2nv s1c s2show s2c
permutation s1upd s2nv s1a s2show s2c

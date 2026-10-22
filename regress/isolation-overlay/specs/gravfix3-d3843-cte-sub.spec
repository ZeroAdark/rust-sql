# gravfix3 D3843: Hibernate's CTE mutation UPDATE of a joined subclass reads a correlated subquery in SET (a per-row
# SubPlan beside the Hash Join of the target's scan and the id CTE). Here it rechecks rows that another session updated
# or deleted. EvalPlanQual runs the SubPlan again over the new row version, under the statement's snapshot. s2g's SubPlans
# are aggregates, each over a rerun set that runs again at the recheck.

setup
{
  CREATE TABLE d3843r (id bigint PRIMARY KEY, kind varchar(255));
  CREATE TABLE d3843s (id bigint PRIMARY KEY REFERENCES d3843r, name varchar(255), ref bigint);
  CREATE TABLE d3843d (id bigint PRIMARY KEY, root_id bigint, tag varchar(255));
  INSERT INTO d3843r VALUES (1, 'a'), (2, 'b'), (3, 'c'), (4, 'd');
  INSERT INTO d3843s VALUES (1, 'n1', 1), (2, 'n2', 2), (4, 'n4', 4);
  INSERT INTO d3843d VALUES (10, 1, 't10'), (20, 2, 't20'), (40, 3, 't40');
}

teardown
{
  DROP TABLE d3843d;
  DROP TABLE d3843s;
  DROP TABLE d3843r;
}

session s1
step s1b { BEGIN; }
step s1u { UPDATE d3843s SET name = name || '+', ref = 3 WHERE id = 1; }
step s1d { DELETE FROM d3843s WHERE id = 2; }
step s1t { UPDATE d3843d SET tag = 'changed' WHERE id = 10; }
step s1c { COMMIT; }
step s1r { ROLLBACK; }

session s2
step s2h { with id_cte (id) as materialized (select sr1_0.id from d3843s sr1_0 join d3843r sr1_1 on sr1_0.id=sr1_1.id), update_cte_SubRoot (id) as (update d3843s sr1_0 set name=(select 'test' from d3843d d1_0 where d1_0.root_id=sr1_0.id) where sr1_0.id in (select id.id from id_cte id) returning id) select count(*) from id_cte id; }
step s2r { WITH id_cte (id) AS MATERIALIZED (SELECT id FROM d3843r WHERE kind <> 'c'), u AS (UPDATE d3843s s SET name = s.name || '/' || coalesce((SELECT d.tag FROM d3843d d WHERE d.root_id = s.ref), '-') WHERE s.id IN (SELECT id FROM id_cte) RETURNING s.id, s.name) SELECT * FROM u ORDER BY id; }
step s2g { WITH id_cte (id) AS MATERIALIZED (SELECT id FROM d3843r WHERE kind <> 'c'), u AS (UPDATE d3843s s SET name = s.name || '/' || coalesce((SELECT max(d.tag) FROM d3843d d WHERE d.root_id = s.ref), '-') || '/' || (SELECT count(*) FROM d3843d d WHERE d.root_id = s.ref)::text WHERE s.id IN (SELECT id FROM id_cte) RETURNING s.id, s.name) SELECT * FROM u ORDER BY id; }
step s2s { SELECT * FROM d3843s ORDER BY id; }

permutation s1b s1u s2h s1c s2s
permutation s1b s1d s2h s1c s2s
permutation s1b s1u s2r s1c s2s
permutation s1b s1u s2r s1r s2s
permutation s1b s1u s1d s2r s1c s2s
permutation s1b s1t s1u s2r s1c s2s
permutation s1b s1u s2g s1c s2s
permutation s1b s1t s1u s2g s1c s2s

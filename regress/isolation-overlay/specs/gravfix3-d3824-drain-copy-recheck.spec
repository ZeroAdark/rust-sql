# D3824 DRAIN-COPY: Hibernate ORM 7's CTE mutation strategy (a bulk DELETE / UPDATE of a joined or union-subclass
# hierarchy: `with id_cte (id) as materialized (..), delete_cte_X (id) as (delete from X where k in (select id from
# id_cte) returning k) select count(*) from id_cte`) with the data-modifying CTE's target hashed under a Hash Right Semi
# Join that the CTE Scan of id_cte probes (the target the smaller side: 4 rows against 1003 ids). Another session
# changes a target row first (READ COMMITTED), so s2's write waits on it, then EvalPlanQual rechecks the newest version:
# it hands the CTE Scan its stored row (the COPY mark) and reruns the join over it and the newest version, which the
# write takes when the hash clause still holds.
#  (a) s1 changes the row's city (its key unchanged): s2's DELETE deletes the newest version, its UPDATE updates it
#  (b) s1 moves the row to a key id_cte does not hold: s2's write skips it, every other row written; to a key id_cte
#      holds but the probe row the join matched does not: skipped too (EvalPlanQual's CTE Scan returns that one stored
#      row, never the CTE's other rows)
#  (c) s1 deletes the row: s2 skips it
#  (d) s1 aborts (control)
# The DELETE CTE's RETURNING rows are read by nothing (Hibernate's form); the UPDATE's are read by the statement (s2upr).
# Each prints the table after s2's write. The expected output is PostgreSQL 18.6's.
setup
{
  CREATE TABLE d3824a (id bigint PRIMARY KEY, description text);
  CREATE TABLE d3824h (human bigint NOT NULL, "type" text NOT NULL, city text, PRIMARY KEY (human, "type"));
  INSERT INTO d3824a SELECT g, 'x' FROM generate_series(1, 3) g UNION ALL SELECT g, 'y' FROM generate_series(100, 1099) g;
  INSERT INTO d3824h VALUES (1, 'home', 'Austin'), (2, 'home', 'Boston'), (2, 'work', 'Boston'), (4, 'home', 'Dallas');
  ANALYZE d3824a; ANALYZE d3824h;
}
teardown { DROP TABLE d3824h, d3824a; }

session s1
setup		{ BEGIN; }
step s1city	{ UPDATE d3824h SET city = 'Waco' WHERE human = 2 AND "type" = 'home'; }
step s1move	{ UPDATE d3824h SET human = 4 WHERE human = 2 AND "type" = 'work'; }
step s1mv3	{ UPDATE d3824h SET human = 3 WHERE human = 2 AND "type" = 'work'; }
step s1del	{ DELETE FROM d3824h WHERE human = 2 AND "type" = 'home'; }
step s1c	{ COMMIT; }
step s1a	{ ROLLBACK; }

session s2
setup		{ BEGIN; }
step s2x	{ EXPLAIN (COSTS OFF) with id_cte (id) as materialized (select a1_0.id from d3824a a1_0), delete_cte_h (id) as (delete from d3824h to_delete_ where to_delete_.human in (select id.id from id_cte id) returning human) select count(*) from id_cte id; }
step s2del	{ with id_cte (id) as materialized (select a1_0.id from d3824a a1_0), delete_cte_h (id) as (delete from d3824h to_delete_ where to_delete_.human in (select id.id from id_cte id) returning human) select count(*) from id_cte id; }
step s2upd	{ with id_cte (id) as materialized (select a1_0.id from d3824a a1_0), update_cte_h (id) as (update d3824h to_update_ set city = city || '+s2' where to_update_.human in (select id.id from id_cte id) returning human) select count(*) from id_cte id; }
step s2upr	{ with id_cte (id) as materialized (select a1_0.id from d3824a a1_0), update_cte_h as (update d3824h to_update_ set city = city || '+s2' where to_update_.human in (select id.id from id_cte id) returning human, "type", city) select human, "type", city from update_cte_h order by 1, 2; }
step s2show	{ SELECT human, "type", city FROM d3824h ORDER BY 1, 2; }
step s2c	{ COMMIT; }

permutation s2x s1city s2del s1c s2show s2c
permutation s1city s2upd s1c s2show s2c
permutation s1city s2upr s1c s2show s2c
permutation s1move s2del s1c s2show s2c
permutation s1move s2upd s1c s2show s2c
permutation s1mv3 s2del s1c s2show s2c
permutation s1mv3 s2upr s1c s2show s2c
permutation s1del s2del s1c s2show s2c
permutation s1del s2upd s1c s2show s2c
permutation s1city s2del s1a s2show s2c

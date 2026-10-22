# D3870 BUILT-HELD: Hibernate ORM 7's CTE mutation strategy over a joined-subclass hierarchy (`with id_cte (id) as
# materialized (select .. from leaf join nonleaf .. join root ..), delete_cte_root (id) as (delete from root where id1_ in
# (select id.id from id_cte id) returning id1_) select count(*) from id_cte id`). With the hierarchy's foreign keys and no
# statistics (Hibernate's own column lists) PostgreSQL 18.6 plans the root's write as a Hash Join whose HashAggregate (CTE Scan id_cte) probes the Hash
# of the target's scan. Another session changes a target row first (READ COMMITTED), so s2's write waits on it, then
# EvalPlanQual rechecks the newest version: it hands the CTE Scan its stored row (the COPY mark), the HashAggregate makes
# that one group again, and the Hash Join joins it to the newest version, which the write takes when the hash clause
# still holds.
#  (a) s1 changes the row's name (its key unchanged): s2's DELETE deletes the newest version, its UPDATE updates it
#  (b) s1 moves the row to another key (ON UPDATE CASCADE): s2's write skips it, the other row written
#  (c) s1 deletes the row: s2 skips it
#  (d) s1 aborts (control)
# The DELETE's and the UPDATE's RETURNING rows read by nothing (Hibernate's form) or by the statement (s2dr, s2ur). Each
# prints the tables after s2's write. The expected output is PostgreSQL 18.6's.
setup
{
  CREATE TABLE d3870root (id1_ bigint NOT NULL, count_ integer NOT NULL, name varchar(255), address varchar(255), date_ timestamp(6), top_ bigint, parent bigint, PRIMARY KEY (id1_));
  CREATE TABLE d3870non (sid bigint NOT NULL, dupe varchar(255), other2 bigint, cal timestamp(6), floaty float4, po bigint, PRIMARY KEY (sid));
  CREATE TABLE d3870leaf (sid bigint NOT NULL, dupe float4, parent bigint, another_parent bigint, list_ind integer, list_po bigint, i integer, PRIMARY KEY (sid));
  ALTER TABLE d3870leaf ADD FOREIGN KEY (sid) REFERENCES d3870non ON DELETE CASCADE ON UPDATE CASCADE;
  ALTER TABLE d3870non ADD FOREIGN KEY (sid) REFERENCES d3870root ON DELETE CASCADE ON UPDATE CASCADE;
  INSERT INTO d3870root (id1_, count_, name) VALUES (1, 0, 'r1'), (2, 0, 'r2'), (3, 0, 'r3'), (4, 0, 'r4'), (5, 0, 'r5');
  INSERT INTO d3870non (sid) VALUES (2), (3), (4), (5);
  INSERT INTO d3870leaf (sid) VALUES (3), (5);
}
teardown { DROP TABLE d3870leaf, d3870non, d3870root; }

session s1
setup		{ BEGIN; }
step s1name	{ UPDATE d3870root SET name = 'n3' WHERE id1_ = 3; }
step s1move	{ UPDATE d3870root SET id1_ = 7 WHERE id1_ = 3; }
step s1del	{ DELETE FROM d3870root WHERE id1_ = 3; }
step s1c	{ COMMIT; }
step s1a	{ ROLLBACK; }

session s2
setup		{ BEGIN; }
step s2x	{ EXPLAIN (COSTS OFF) with id_cte (id) as materialized (select l.sid from d3870leaf l join d3870non n on l.sid = n.sid join d3870root r on l.sid = r.id1_), delete_cte_root (id) as (delete from d3870root r2 where r2.id1_ in (select id.id from id_cte id) returning id1_) select count(*) from id_cte id; }
step s2d	{ with id_cte (id) as materialized (select l.sid from d3870leaf l join d3870non n on l.sid = n.sid join d3870root r on l.sid = r.id1_), delete_cte_root (id) as (delete from d3870root r2 where r2.id1_ in (select id.id from id_cte id) returning id1_) select count(*) from id_cte id; }
step s2dr	{ with id_cte (id) as materialized (select l.sid from d3870leaf l join d3870non n on l.sid = n.sid join d3870root r on l.sid = r.id1_), d as (delete from d3870root r2 where r2.id1_ in (select id.id from id_cte id) returning id1_, name) select * from d order by 1; }
step s2u	{ with id_cte (id) as materialized (select l.sid from d3870leaf l join d3870non n on l.sid = n.sid join d3870root r on l.sid = r.id1_), update_cte_root (id) as (update d3870root r2 set name = name || '+s2' where r2.id1_ in (select id.id from id_cte id) returning id1_) select count(*) from id_cte id; }
step s2ur	{ with id_cte (id) as materialized (select l.sid from d3870leaf l join d3870non n on l.sid = n.sid join d3870root r on l.sid = r.id1_), u as (update d3870root r2 set name = name || '+s2' where r2.id1_ in (select id.id from id_cte id) returning id1_, name) select * from u order by 1; }
step s2show	{ SELECT r.id1_, r.name, n.sid AS non, l.sid AS leaf FROM d3870root r LEFT JOIN d3870non n ON n.sid = r.id1_ LEFT JOIN d3870leaf l ON l.sid = r.id1_ ORDER BY 1; }
step s2c	{ COMMIT; }

permutation s2x s1name s2d s1c s2show s2c
permutation s1name s2dr s1c s2show s2c
permutation s1name s2u s1c s2show s2c
permutation s1name s2ur s1c s2show s2c
permutation s1move s2d s1c s2show s2c
permutation s1move s2ur s1c s2show s2c
permutation s1del s2dr s1c s2show s2c
permutation s1del s2u s1c s2show s2c
permutation s1name s2d s1a s2show s2c

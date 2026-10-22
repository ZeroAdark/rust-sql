# D3825: Hibernate ORM 7's DeleteWithSubqueryTest DELETE (`delete from Panel p1_0 where p1_0.clientId in (select
# tp1_0.clientId from TrtPanel tp1_0) and p1_0.deltaStamp not in (select tp2_0.deltaStamp from TrtPanel tp2_0)`) with the
# target the smaller side: a Hash Right Semi Join whose build, the target's scan, filters by the hashed NOT IN SubPlan
# (4 rows against 2000). Another session changes a target row first (READ COMMITTED), so s2's write waits on it, then
# EvalPlanQual rechecks the newest version: the build's filter again over the NOT IN set (built again from the same rows
# under the statement's snapshot), and the join's hash clause against the probe row it matched (TrtPanel's REFERENCE
# mark refetches that row).
#  (a) s1 changes the row's name: s2's DELETE deletes the newest version, its UPDATE updates it
#  (b) s1 sets the row's stamp to one the NOT IN set holds: s2 skips it
#  (c) s1 moves the row's client to one no TrtPanel row holds: skipped; to one another TrtPanel row holds: skipped too
#      (the recheck meets only the probe row the join matched)
#  (d) s1 deletes the row: s2 skips it
#  (e) s1 aborts (control)
# Each prints the table after s2's write. The expected output is PostgreSQL 18.6's.
setup
{
  CREATE TABLE d3825p (id bigint PRIMARY KEY, clientid bigint, deltastamp timestamp, name text);
  CREATE TABLE d3825t (id bigint PRIMARY KEY, clientid bigint, deltastamp timestamp);
  INSERT INTO d3825p VALUES (1, 10, '2020-01-01', 'a'), (2, 20, '2020-01-02', 'b'), (3, 30, '2020-01-03', 'c'), (4, 10, '2020-01-04', 'd');
  INSERT INTO d3825t VALUES (1, 10, '2021-01-01'), (2, 20, '2020-01-02');
  INSERT INTO d3825t SELECT g, 1000 + g, timestamp '2022-01-01' + g * interval '1 hour' FROM generate_series(3, 2000) g;
  ANALYZE d3825p; ANALYZE d3825t;
}
teardown { DROP TABLE d3825p, d3825t; }

session s1
setup		{ BEGIN; }
step s1name	{ UPDATE d3825p SET name = 'z' WHERE id = 1; }
step s1stamp	{ UPDATE d3825p SET deltastamp = '2021-01-01' WHERE id = 1; }
step s1client	{ UPDATE d3825p SET clientid = 30 WHERE id = 1; }
step s1cl3	{ UPDATE d3825p SET clientid = 1003 WHERE id = 1; }
step s1del	{ DELETE FROM d3825p WHERE id = 1; }
step s1c	{ COMMIT; }
step s1a	{ ROLLBACK; }

session s2
setup		{ BEGIN; }
step s2x	{ EXPLAIN (COSTS OFF) delete from d3825p p1_0 where p1_0.clientid in (select tp1_0.clientid from d3825t tp1_0) and p1_0.deltastamp not in (select tp2_0.deltastamp from d3825t tp2_0); }
step s2del	{ delete from d3825p p1_0 where p1_0.clientid in (select tp1_0.clientid from d3825t tp1_0) and p1_0.deltastamp not in (select tp2_0.deltastamp from d3825t tp2_0) returning p1_0.id, p1_0.name; }
step s2upd	{ update d3825p p1_0 set name = name || '+s2' where p1_0.clientid in (select tp1_0.clientid from d3825t tp1_0) and p1_0.deltastamp not in (select tp2_0.deltastamp from d3825t tp2_0) returning p1_0.id, p1_0.name; }
step s2show	{ SELECT * FROM d3825p ORDER BY id; }
step s2c	{ COMMIT; }

permutation s2x s1name s2del s1c s2show s2c
permutation s1name s2upd s1c s2show s2c
permutation s1stamp s2del s1c s2show s2c
permutation s1stamp s2upd s1c s2show s2c
permutation s1client s2del s1c s2show s2c
permutation s1cl3 s2del s1c s2show s2c
permutation s1cl3 s2upd s1c s2show s2c
permutation s1del s2del s1c s2show s2c
permutation s1name s2del s1a s2show s2c

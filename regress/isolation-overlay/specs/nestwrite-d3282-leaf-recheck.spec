# D3282 (lowerv4 ops/modifytable/nest_write.rs `dml_form`): an UPDATE / DELETE .. FROM (VALUES ..) through a
# list-partitioned root the planner prunes to one leaf, its rows a nested loop of the VALUES list (the outer, COPY-marked)
# and the leaf's scan (the inner), while another session writes the same row (READ COMMITTED). EvalPlanQual hands the
# VALUES row back from its copy and the leaf's scan the newest version alone; the inner's filter and the join qual run
# again over it. The e2e twin is e2e/gravity/v5/write_d3282_nest_routed_leaf.sql.
#  keep:  s1 changes b in place: s2 writes the newest version (a move to the leaf of 3, an in-place SET, a delete)
#  away:  s1 moves the row to 4 inside its leaf: the filter fails on the newest version, s2 skips the row
#  moved: s1 moves the row to another leaf: s2's lock finds it moved and raises 40001
#  gone:  s1 deletes the row: s2 skips it
setup
{
  CREATE TABLE r3282 (a int, b int) PARTITION BY LIST (a);
  CREATE TABLE r3282_1 PARTITION OF r3282 FOR VALUES IN (1, 4);
  CREATE TABLE r3282_2 PARTITION OF r3282 FOR VALUES IN (2);
  CREATE TABLE r3282_3 PARTITION OF r3282 FOR VALUES IN (3);
  INSERT INTO r3282 VALUES (1, 1), (1, 2), (2, 3);
}
teardown { DROP TABLE r3282; }

session s1
step b1 { BEGIN; }
step keep { UPDATE r3282 SET b = b + 10 WHERE b = 1; }
step away { UPDATE r3282 SET a = 4 WHERE b = 1; }
step moved { UPDATE r3282 SET a = 2 WHERE b = 1; }
step gone { DELETE FROM r3282 WHERE b = 1; }
step c1 { COMMIT; }

session s2
step mv { UPDATE r3282 t SET a = 3 FROM (VALUES (1), (2)) s(a) WHERE t.a = s.a AND s.a = 1; }
step st { UPDATE r3282 t SET b = t.b * 100 FROM (VALUES (1), (2)) s(a) WHERE t.a = s.a AND s.a = 1; }
step del { DELETE FROM r3282 t USING (VALUES (1), (2)) s(a) WHERE t.a = s.a AND s.a = 1; }
step sel { SELECT tableoid::regclass, a, b FROM r3282 ORDER BY 1, 2, 3; }

permutation b1 keep mv c1 sel
permutation b1 away mv c1 sel
permutation b1 moved mv c1 sel
permutation b1 gone mv c1 sel
permutation b1 keep st c1 sel
permutation b1 away st c1 sel
permutation b1 keep del c1 sel
permutation b1 moved del c1 sel

# D3373 (4) UNION-SUBS (modifytable_union_subs): an UPDATE of an inheritance tree whose SET list reads a correlated SubPlan,
# planned as ModifyTable over a Result (the SET list, SubPlan 1) over the Append of ih and ih_c. Gravity keeps the union
# whole and runs the SubPlan above it. Over a newer version of a target row (READ COMMITTED) PostgreSQL's EvalPlanQual
# reruns the row's own relation's scan over the newest version (the other child's blocked), its WHERE, then the Result:
# the SubPlan again with the newest version's d, under the statement's snapshot (s1's change to ihs, committed after s2's
# snapshot, is not seen). A row s1 moved out of the WHERE is skipped; a row s1 deleted is skipped. s2ur: RETURNING after
# the write, a rechecked row's values the newest version's, in the Append's order. Expected output is PostgreSQL 18.6's.
setup
{
  CREATE TABLE ih (id int, d int, v int);
  CREATE TABLE ih_c (extra int DEFAULT 7) INHERITS (ih);
  INSERT INTO ih SELECT i, i % 5, i FROM generate_series(1, 40) i;
  INSERT INTO ih_c (id, d, v) SELECT i, i % 5, i FROM generate_series(41, 90) i;
  CREATE TABLE ihs (d int, w int);
  INSERT INTO ihs SELECT i % 7, i * 3 FROM generate_series(1, 60) i;
  ANALYZE ih, ih_c, ihs;
}
teardown { DROP TABLE ih_c, ih, ihs; }

session s1
setup		{ BEGIN; }
step s1d	{ UPDATE ih SET d = 6 WHERE id IN (3, 42); }
step s1v	{ UPDATE ih_c SET v = v + 1000, d = d + 1 WHERE id IN (45, 48); }
step s1x	{ DELETE FROM ih WHERE id IN (6, 51); UPDATE ih SET id = id + 1 WHERE id IN (9, 54); }
step s1s	{ INSERT INTO ihs VALUES (6, 1), (6, 2), (0, 3); }
step s1c	{ COMMIT; }

session s2
setup		{ SET max_parallel_workers_per_gather = 0; }
step s2u	{ UPDATE ih SET v = (SELECT count(*) FROM ihs WHERE ihs.d = ih.d) * 100 + v WHERE ih.id % 3 = 0; }
step s2ur	{ UPDATE ih SET v = (SELECT count(*) FROM ihs WHERE ihs.d = ih.d) * 100 + v WHERE ih.id % 9 = 0 RETURNING tableoid::regclass, id, d, v; }
step s2r	{ SELECT tableoid::regclass, id, d, v FROM ih WHERE id % 3 = 0 OR id IN (45, 48, 10, 55) ORDER BY id, d, v; }

permutation s1d s2u s1c s2r
permutation s1v s2u s1c s2r
permutation s1x s2u s1c s2r
permutation s1d s1s s2u s1c s2r
permutation s1s s2u s1c s2r
permutation s1v s2ur s1c s2r
permutation s1x s2ur s1c s2r

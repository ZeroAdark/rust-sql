# D3714 (gravfix3_d3297_update_arms :62): an UPDATE of a partitioned target's leaves (a nest per leaf arm over one plan
# Material) whose Material reads a second partitioned table pruned at run time over an InitPlan that may raise (3 /
# count(*)), here answering 1. PostgreSQL runs the InitPlan at the Append's first call, before any row reaches the write;
# its EvalPlanQual over a newer version of a target row (READ COMMITTED) copies that value down and runs nothing again: a
# row s1 changed is updated over its newest version, a row s1 deleted is skipped, a row s1 moved to another leaf of the
# target raises 40001, with an in-place SET (s2u) and with one that moves rows too (s2b). The InitPlan's scan runs under
# the statement's snapshot: rows s1 adds to ri_w stay unseen. Expected output is PostgreSQL 18.6's.
setup
{
  CREATE TABLE ri (a int NOT NULL, b int NOT NULL, v int NOT NULL) PARTITION BY LIST (a);
  CREATE TABLE ri_a1 PARTITION OF ri FOR VALUES IN (1) PARTITION BY LIST (b);
  CREATE TABLE ri_a1_b1 PARTITION OF ri_a1 FOR VALUES IN (1);
  CREATE TABLE ri_a1_b2 PARTITION OF ri_a1 FOR VALUES IN (2);
  CREATE TABLE ri_a1_b3 PARTITION OF ri_a1 FOR VALUES IN (3);
  CREATE TABLE ri_a2 PARTITION OF ri FOR VALUES IN (2) PARTITION BY LIST (b);
  CREATE TABLE ri_a2_b1 PARTITION OF ri_a2 FOR VALUES IN (1);
  CREATE TABLE ri_a2_b2 PARTITION OF ri_a2 FOR VALUES IN (2);
  CREATE TABLE ri_w (x int);
  INSERT INTO ri_w VALUES (1), (2), (3);
  INSERT INTO ri VALUES (1, 1, 10), (1, 2, 20), (2, 1, 0), (2, 2, 0);
}
teardown { DROP TABLE ri, ri_w; }

session s1
setup		{ BEGIN; }
step s1v	{ UPDATE ri SET v = v + 1 WHERE a = 1 AND b = 1; }
step s1m	{ UPDATE ri SET b = 3 WHERE a = 1 AND b = 1; }
step s1d	{ DELETE FROM ri WHERE a = 1 AND b = 1; }
step s1w	{ INSERT INTO ri_w VALUES (4), (5), (6); }
step s1c	{ COMMIT; }

session s2
setup		{ SET max_parallel_workers_per_gather = 0; SET plan_cache_mode = force_generic_plan; }
step s2u	{ UPDATE ri_a1 SET v = ri_a1.v + 100 FROM ri_a2 WHERE ri_a2.b = (SELECT 3 / count(*)::int FROM ri_w); }
step s2b	{ UPDATE ri_a1 SET b = 2, v = ri_a1.v + 100 FROM ri_a2 WHERE ri_a2.b = (SELECT 3 / count(*)::int FROM ri_w); }
step s2r	{ SELECT tableoid::regclass AS part, * FROM ri ORDER BY a, b, v; }

permutation s1v s2u s1c s2r
permutation s1d s2u s1c s2r
permutation s1m s2u s1c s2r
permutation s1v s2b s1c s2r
permutation s1m s2b s1c s2r
permutation s1v s1w s2u s1c s2r

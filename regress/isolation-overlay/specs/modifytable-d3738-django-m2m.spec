# D3738: Django's update() / delete() over an m2m-annotated queryset (`pk IN (SELECT U0.id FROM bar U0 LEFT OUTER JOIN
# bar_m2m U1 ON (U0.id = U1.bar_id))`) over fresh tables, PostgreSQL 18.6's Hash Right Semi Join with the target hashed
# whose probe is a Hash Right Join (bar_m2m probing, U0 hashed), against a concurrent change of a target row (READ
# COMMITTED). The write finds the version its snapshot read updated and rechecks the newest one under EvalPlanQual: the
# probe side's Hash Right Join reruns over U0's and U1's refetched rows (U1's none when the row came null-extended), so
# a row whose other columns changed is written over its newest version, one whose key moved is skipped (U0's old
# version holds the old key), a deleted one is gone. The steps return no rows (the drained rows' order is free,
# DECISIONS 6b); the table after each shows what was written. The expected output is PostgreSQL 18.6's.
setup
{
  CREATE TABLE bar (id serial PRIMARY KEY, x int);
  CREATE TABLE foo (id serial PRIMARY KEY, target text);
  CREATE TABLE bar_m2m (id serial PRIMARY KEY, bar_id int REFERENCES bar ON UPDATE CASCADE ON DELETE CASCADE, foo_id int REFERENCES foo, UNIQUE (bar_id, foo_id));
  INSERT INTO bar (x) SELECT i FROM generate_series(1, 6) i;
  INSERT INTO foo (target) VALUES ('a'), ('b'), ('c');
  INSERT INTO bar_m2m (bar_id, foo_id) VALUES (1, 1), (1, 2), (2, 3), (4, 1);
}
teardown { DROP TABLE bar_m2m, foo, bar; }

session s1
setup		{ BEGIN; }
step s1x3	{ UPDATE bar SET x = x + 100 WHERE id = 3; }
step s1x1	{ UPDATE bar SET x = x + 100 WHERE id = 1; }
step s1key	{ UPDATE bar SET id = 99 WHERE id = 5; }
step s1del	{ DELETE FROM bar WHERE id = 6; }
step s1m2m	{ DELETE FROM bar_m2m WHERE bar_id = 2; }
step s1c	{ COMMIT; }

session s2
step s2x	{ EXPLAIN (COSTS OFF) UPDATE bar SET x = x * 2 WHERE bar.id IN (SELECT U0.id FROM bar U0 LEFT OUTER JOIN bar_m2m U1 ON (U0.id = U1.bar_id)); }
step s2upd	{ UPDATE bar SET x = x * 2 WHERE bar.id IN (SELECT U0.id FROM bar U0 LEFT OUTER JOIN bar_m2m U1 ON (U0.id = U1.bar_id)); }
step s2updn	{ UPDATE bar SET x = -x WHERE bar.id IN (SELECT U0.id FROM bar U0 LEFT OUTER JOIN bar_m2m U1 ON (U0.id = U1.bar_id) WHERE U1.foo_id IS DISTINCT FROM 1); }
step s2del	{ DELETE FROM bar WHERE bar.id IN (SELECT U0.id FROM bar U0 LEFT OUTER JOIN bar_m2m U1 ON (U0.id = U1.bar_id) WHERE U1.foo_id IS DISTINCT FROM 1); }
step s2show	{ SELECT id, x FROM bar ORDER BY id; }

permutation s2x s1x3 s2upd s1c s2show
permutation s1x1 s2upd s1c s2show
permutation s1key s2upd s1c s2show
permutation s1del s2upd s1c s2show
permutation s1x3 s2updn s1c s2show
permutation s1m2m s2updn s1c s2show
permutation s1key s2del s1c s2show

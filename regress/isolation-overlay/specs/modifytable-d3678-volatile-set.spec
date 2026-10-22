# D3678: an UPDATE / DELETE whose qual probes a hashed SubPlan (an uncorrelated NOT IN) whose plan calls a VOLATILE
# function (nextval), against a concurrent change of target rows (READ COMMITTED). The write finds the version its
# snapshot read updated and rechecks the newest one under EvalPlanQual, whose fresh SubPlan state builds the set again,
# the VOLATILE call run again for every subquery row: the session's currval counts the runs. A row changed but still
# outside the set is written over its newest version, one whose key moved into the set is skipped, a deleted one is
# gone. The expected output is PostgreSQL 18.6's.
setup
{
  CREATE TABLE vs_prod (id int PRIMARY KEY, grp int);
  INSERT INTO vs_prod SELECT i, i % 4 FROM generate_series(1, 20) i;
  CREATE TABLE vs_inv (id int PRIMARY KEY, product_id int, qty int NOT NULL);
  INSERT INTO vs_inv SELECT i, i, i FROM generate_series(15, 30) i;
  CREATE SEQUENCE vs_seq;
  ANALYZE vs_prod; ANALYZE vs_inv;
}
teardown { DROP TABLE vs_prod, vs_inv; DROP SEQUENCE vs_seq; }

session s1
setup		{ BEGIN; }
step s1bump	{ UPDATE vs_inv SET qty = qty + 100 WHERE id IN (22, 25); }
step s1in	{ UPDATE vs_inv SET product_id = 3, qty = qty + 100 WHERE id = 22; }
step s1del	{ DELETE FROM vs_inv WHERE id = 22; }
step s1c	{ COMMIT; }

session s2
step s2x	{ EXPLAIN (COSTS OFF) DELETE FROM vs_inv WHERE product_id NOT IN (SELECT id FROM vs_prod WHERE nextval('vs_seq') > 0) RETURNING id, product_id, qty; }
step s2del	{ DELETE FROM vs_inv WHERE product_id NOT IN (SELECT id FROM vs_prod WHERE nextval('vs_seq') > 0) RETURNING id, product_id, qty; }
step s2upd	{ UPDATE vs_inv SET qty = -qty WHERE product_id NOT IN (SELECT id FROM vs_prod WHERE nextval('vs_seq') > 0) RETURNING id, qty; }
step s2seq	{ SELECT currval('vs_seq'); }
step s2show	{ SELECT id, product_id, qty FROM vs_inv ORDER BY id; }

permutation s2x s2del s2seq s2show s1c
permutation s1bump s2del s1c s2seq s2show
permutation s1in s2del s1c s2seq s2show
permutation s1del s2upd s1c s2seq s2show
permutation s1bump s2upd s1c s2seq s2show

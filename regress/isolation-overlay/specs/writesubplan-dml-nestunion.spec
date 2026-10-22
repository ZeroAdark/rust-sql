# writesubplan (regress subselect#137): a DELETE / UPDATE of an inheritance parent and its children (several result
# relations) over a Nested Loop whose outer is the HashAggregate making an EXISTS semi join's inner side unique and whose
# inner is the Append of the parent's seq scan and the children's bitmap heap scans, parameterized by the outer, while
# another session changes a target row (READ COMMITTED). PostgreSQL 18.6's EvalPlanQual substitutes the row's own
# relation's scan with the newest version (its bitmap Recheck Cond / Filter rechecked) and blocks the other arms:
#  (a) s1 bumps x of every k1 row (parent, both children): each is still deleted
#  (b) s1 renames a child's k2 row: its recheck fails, the row stays
#  (c) s1 deletes a child's k2 row: nothing to delete there
#  (d) s1 renames the parent's k1 row to k3 (k3 is another group's key): its recheck fails against its own group, the row stays
#  (e) s1 aborts: the original versions (control)
#  (f) (a) and (d) with RETURNING tableoid and the row, over a source holding the one key k1 (wdn_t1), so the rows come
#      out of one group in the Append's order
#  (g) the UPDATE of the same form over (a) and (b)
# s2 plans with nested loops alone; each permutation ends s2's transaction. Expected output is PostgreSQL 18.6's (the C oracle).
setup
{
 CREATE TABLE wdn_r (name text, x int);
 CREATE TABLE wdn_i (y int) INHERITS (wdn_r);
 CREATE TABLE wdn_s (z text) INHERITS (wdn_r);
 INSERT INTO wdn_r SELECT 'p' || (g % 50), g FROM generate_series(1, 200) g;
 INSERT INTO wdn_i SELECT 'i' || (g % 400), g, g % 7 FROM generate_series(1, 4000) g;
 INSERT INTO wdn_s SELECT 's' || (g % 400), g, 'z' || g FROM generate_series(1, 4000) g;
 INSERT INTO wdn_i VALUES ('k1', 1, 1), ('k2', 2, 2), ('k3', 3, 3);
 INSERT INTO wdn_s VALUES ('k1', 11, 'a'), ('k2', 12, 'b');
 INSERT INTO wdn_r VALUES ('k1', 21), ('k3', 23);
 CREATE INDEX ON wdn_i (name);
 CREATE INDEX ON wdn_s (name);
 CREATE TABLE wdn_a (f1 int);
 INSERT INTO wdn_a VALUES (1), (2), (3), (4), (5);
 CREATE TABLE wdn_t (f1 text);
 INSERT INTO wdn_t VALUES ('k1'), ('k2'), ('k3'), ('nope');
 CREATE TABLE wdn_t1 (f1 text);
 INSERT INTO wdn_t1 VALUES ('k1'), ('nope');
 CREATE TABLE wdn_q (q1 int8);
 INSERT INTO wdn_q VALUES (10), (20);
 ANALYZE wdn_r, wdn_i, wdn_s, wdn_a, wdn_t, wdn_t1, wdn_q;
}

teardown
{
 DROP TABLE wdn_r, wdn_i, wdn_s, wdn_a, wdn_t, wdn_t1, wdn_q;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1x	{ UPDATE wdn_r SET x = x + 100 WHERE name = 'k1'; }
step s1ren	{ UPDATE wdn_i SET name = 'gone' WHERE name = 'k2'; }
step s1del	{ DELETE FROM wdn_s WHERE name = 'k2'; }
step s1key	{ UPDATE ONLY wdn_r SET name = 'k3' WHERE name = 'k1'; }
step s1c	{ COMMIT; }
step s1a	{ ROLLBACK; }

session s2
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; SET LOCAL enable_hashjoin = off; SET LOCAL enable_mergejoin = off; }
step s2e	{ EXPLAIN (COSTS OFF) DELETE FROM wdn_r WHERE EXISTS (SELECT 1 FROM wdn_a CROSS JOIN (SELECT f1, array(SELECT q1 FROM wdn_q) AS arr FROM wdn_t) ss WHERE wdn_r.name = ss.f1); }
step s2d	{ DELETE FROM wdn_r WHERE EXISTS (SELECT 1 FROM wdn_a CROSS JOIN (SELECT f1, array(SELECT q1 FROM wdn_q) AS arr FROM wdn_t) ss WHERE wdn_r.name = ss.f1); }
step s2r	{ DELETE FROM wdn_r WHERE EXISTS (SELECT 1 FROM wdn_a CROSS JOIN (SELECT f1, array(SELECT q1 FROM wdn_q) AS arr FROM wdn_t1) ss WHERE wdn_r.name = ss.f1) RETURNING tableoid::regclass, name, x; }
step s2re	{ EXPLAIN (COSTS OFF) DELETE FROM wdn_r WHERE EXISTS (SELECT 1 FROM wdn_a CROSS JOIN (SELECT f1, array(SELECT q1 FROM wdn_q) AS arr FROM wdn_t1) ss WHERE wdn_r.name = ss.f1) RETURNING tableoid::regclass, name, x; }
step s2u	{ UPDATE wdn_r SET x = x + 1000 WHERE EXISTS (SELECT 1 FROM wdn_a CROSS JOIN (SELECT f1 FROM wdn_t) ss WHERE wdn_r.name = ss.f1); }
step s2sel	{ SELECT tableoid::regclass, name, x FROM wdn_r WHERE name IN ('k1', 'k2', 'k3', 'gone') ORDER BY 1, 2, 3; }
step s2c	{ COMMIT; }

permutation s2e s1x s2d s1c s2sel s2c
permutation s1ren s2d s1c s2sel s2c
permutation s1del s2d s1c s2sel s2c
permutation s1key s2d s1c s2sel s2c
permutation s1x s2d s1a s2sel s2c
permutation s2re s1x s2r s1c s2sel s2c
permutation s1key s2r s1c s2sel s2c
permutation s1x s2u s1c s2sel s2c
permutation s1ren s2u s1c s2sel s2c

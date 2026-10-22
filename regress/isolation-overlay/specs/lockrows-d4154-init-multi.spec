# D4154 INITMULTI: the M4 pair whose outer LockRows (B) locks over a join (x JOIN g): B locks each row's marks in
# PlanRowMark order and rechecks the join under EvalPlanQual (READ COMMITTED) over the newest versions of the rows it
# waited for, the InitPlan's value copied (attached at the LockRows); A rechecks its own room row inside the InitPlan.
# A guest updated (its newest version joined), its key updated (the join no longer matches), deleted (the row skipped);
# a reservation moved out of the filter; A's room updated under FOR SHARE; NOWAIT and SKIP LOCKED on B; OF x with the
# guest a REFERENCE relation (its row re-fetched, never locked). Expected output is PostgreSQL 18.6's.
setup
{
  CREATE TABLE imr (id int PRIMARY KEY, valid int4range NOT NULL);
  CREATE TABLE img (id int PRIMARY KEY, name text NOT NULL);
  CREATE TABLE imx (id int PRIMARY KEY, room int NOT NULL, guest int NOT NULL, valid int4range);
  INSERT INTO imr VALUES (1, int4range(10, 40)), (2, int4range(20, 50)), (3, int4range(30, 60));
  INSERT INTO img VALUES (1, 'ann'), (2, 'bob'), (3, 'cy');
  INSERT INTO imx VALUES (1, 1, 1, int4range(15, 20)), (2, 2, 2, int4range(45, 55)), (3, 3, 3, int4range(5, 10)), (4, 1, 2, int4range(35, 45)), (5, 2, 1, int4range(55, 70));
}
teardown { DROP TABLE imx, img, imr; }

session s1
setup		{ BEGIN; }
step s1g	{ UPDATE img SET name = 'cyrus' WHERE id = 3; }
step s1gk	{ UPDATE img SET id = 30 WHERE id = 3; }
step s1gd	{ DELETE FROM img WHERE id = 3; }
step s1x	{ UPDATE imx SET room = 9 WHERE id = 3; }
step s1r	{ UPDATE imr SET valid = int4range(0, 70) WHERE id = 1; }
step s1l	{ SELECT id FROM img WHERE id = 3 FOR UPDATE; }
step s1c	{ COMMIT; }
teardown	{ ROLLBACK; }

session s2
step s2a	{ SELECT x.id, g.name FROM imx x JOIN img g ON g.id = x.guest WHERE x.room <= 3 AND NOT coalesce(x.valid <@ (SELECT range_agg(r) FROM (SELECT y.valid r FROM imr y WHERE y.id <= 3 FOR KEY SHARE OF y) y2), false) FOR UPDATE; }
step s2o	{ SELECT x.id, g.name FROM imx x JOIN img g ON g.id = x.guest WHERE x.room <= 3 AND NOT coalesce(x.valid <@ (SELECT range_agg(r) FROM (SELECT y.valid r FROM imr y WHERE y.id <= 3 FOR KEY SHARE OF y) y2), false) FOR UPDATE OF x; }
step s2s	{ SELECT x.id, g.name FROM imx x JOIN img g ON g.id = x.guest WHERE x.room <= 3 AND NOT coalesce(x.valid <@ (SELECT range_agg(r) FROM (SELECT y.valid r FROM imr y WHERE y.id <= 3 FOR SHARE OF y) y2), false) FOR UPDATE; }
step s2n	{ SELECT x.id, g.name FROM imx x JOIN img g ON g.id = x.guest WHERE x.room <= 3 AND NOT coalesce(x.valid <@ (SELECT range_agg(r) FROM (SELECT y.valid r FROM imr y WHERE y.id <= 3 FOR KEY SHARE OF y) y2), false) FOR UPDATE NOWAIT; }
step s2k	{ SELECT x.id, g.name FROM imx x JOIN img g ON g.id = x.guest WHERE x.room <= 3 AND NOT coalesce(x.valid <@ (SELECT range_agg(r) FROM (SELECT y.valid r FROM imr y WHERE y.id <= 3 FOR KEY SHARE OF y) y2), false) FOR UPDATE SKIP LOCKED; }

permutation s1g s2a s1c
permutation s1gk s2a s1c
permutation s1gd s2a s1c
permutation s1x s2a s1c
permutation s1g s2o s1c
permutation s1x s2o s1c
permutation s1r s2s s1c
permutation s1l s2n s1c
permutation s1l s2k s1c

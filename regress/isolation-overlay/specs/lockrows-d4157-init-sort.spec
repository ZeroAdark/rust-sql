# D4157 INITSORT: the M4 pair with ORDER BY under the outer LockRows (B over a Sort): B locks each sorted row and rechecks
# under EvalPlanQual (READ COMMITTED) over the newest version of the row it waited for, the Sort over that test tuple and
# the InitPlan's value copied, returning the row in its sorted place (its old sort key's place); A rechecks its own room
# row inside the InitPlan. A reservation's sort key changed, moved out of the filter, deleted; A's room widened under FOR
# SHARE; NOWAIT and SKIP LOCKED on B. Expected output is PostgreSQL 18.6's.
setup
{
  CREATE TABLE isr (id int PRIMARY KEY, valid int4range NOT NULL);
  CREATE TABLE isx (id int PRIMARY KEY, room int NOT NULL, rank int NOT NULL, valid int4range);
  INSERT INTO isr VALUES (1, int4range(10, 40)), (2, int4range(20, 50)), (3, int4range(30, 60));
  INSERT INTO isx VALUES (1, 1, 50, int4range(15, 20)), (2, 2, 40, int4range(55, 65)), (3, 3, 30, int4range(5, 10)), (4, 1, 20, int4range(35, 45)), (5, 2, 10, int4range(58, 70));
}
teardown { DROP TABLE isx, isr; }

session s1
setup		{ BEGIN; }
step s1k	{ UPDATE isx SET rank = 99 WHERE id = 3; }
step s1x	{ UPDATE isx SET room = 9 WHERE id = 3; }
step s1d	{ DELETE FROM isx WHERE id = 5; }
step s1r	{ UPDATE isr SET valid = int4range(0, 70) WHERE id = 1; }
step s1l	{ SELECT id FROM isx WHERE id = 3 FOR UPDATE; }
step s1c	{ COMMIT; }
teardown	{ ROLLBACK; }

session s2
step s2a	{ SELECT x.id, x.rank FROM isx x WHERE x.room <= 3 AND NOT coalesce(x.valid <@ (SELECT range_agg(r) FROM (SELECT y.valid r FROM isr y WHERE y.id <= 3 FOR KEY SHARE OF y) y2), false) ORDER BY x.rank FOR UPDATE OF x; }
step s2s	{ SELECT x.id, x.rank FROM isx x WHERE x.room <= 3 AND NOT coalesce(x.valid <@ (SELECT range_agg(r) FROM (SELECT y.valid r FROM isr y WHERE y.id <= 3 FOR SHARE OF y) y2), false) ORDER BY x.rank FOR UPDATE OF x; }
step s2n	{ SELECT x.id, x.rank FROM isx x WHERE x.room <= 3 AND NOT coalesce(x.valid <@ (SELECT range_agg(r) FROM (SELECT y.valid r FROM isr y WHERE y.id <= 3 FOR KEY SHARE OF y) y2), false) ORDER BY x.rank FOR UPDATE OF x NOWAIT; }
step s2k	{ SELECT x.id, x.rank FROM isx x WHERE x.room <= 3 AND NOT coalesce(x.valid <@ (SELECT range_agg(r) FROM (SELECT y.valid r FROM isr y WHERE y.id <= 3 FOR KEY SHARE OF y) y2), false) ORDER BY x.rank FOR UPDATE OF x SKIP LOCKED; }

permutation s1k s2a s1c
permutation s1x s2a s1c
permutation s1d s2a s1c
permutation s1r s2s s1c
permutation s1l s2n s1c
permutation s1l s2k s1c

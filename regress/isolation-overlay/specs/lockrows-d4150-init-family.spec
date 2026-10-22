# D4150 INITFAM: PostgreSQL 18's temporal-FK RI restrict check over a partitioned PK table: LockRows (B) over the
# reservations beside InitPlan 1, whose Aggregate (range_agg) reads LockRows (A) over the rooms' Append, its two leaves.
# Each LockRows rechecks under its own EvalPlanQual (READ COMMITTED): A over the leaf of the row it waited for (its
# newest version through the leaf scan's filter on the way to the Aggregate, a deleted one skipped, one moved to another
# leaf 40001), B over its newest reservation with the InitPlan's value copied. A non-key UPDATE of a room does not block
# FOR KEY SHARE; FOR SHARE waits for it. NOWAIT and SKIP LOCKED inside the InitPlan. Expected output is PostgreSQL 18.6's.
setup
{
  CREATE TABLE ifr (id int NOT NULL, valid int4range NOT NULL, note text, PRIMARY KEY (id)) PARTITION BY RANGE (id);
  CREATE TABLE ifr_1 PARTITION OF ifr FOR VALUES FROM (1) TO (11);
  CREATE TABLE ifr_2 PARTITION OF ifr FOR VALUES FROM (11) TO (21);
  INSERT INTO ifr VALUES (1, int4range(10, 40), 'a'), (2, int4range(20, 50), 'a'), (12, int4range(120, 150), 'b'), (13, int4range(130, 160), 'b');
  CREATE TABLE ifx (id int PRIMARY KEY, room int NOT NULL, valid int4range);
  INSERT INTO ifx VALUES (1, 1, int4range(15, 20)), (2, 2, int4range(25, 45)), (3, 12, int4range(125, 135)), (4, 13, int4range(140, 155)), (5, 12, int4range(100, 110));
}
teardown { DROP TABLE ifx, ifr; }

session s1
setup		{ BEGIN; }
step s1u	{ UPDATE ifr SET valid = int4range(120, 128) WHERE id = 12; }
step s1k	{ UPDATE ifr SET id = 14 WHERE id = 13; }
step s1kv	{ UPDATE ifr SET id = 11, valid = int4range(120, 128) WHERE id = 12; }
step s1d	{ DELETE FROM ifr WHERE id = 12; }
step s1m	{ UPDATE ifr SET id = 3 WHERE id = 12; }
step s1x	{ UPDATE ifx SET id = 40 WHERE id = 5; }
step s1xd	{ DELETE FROM ifx WHERE id = 5; }
step s1l	{ SELECT id FROM ifr WHERE id = 12 FOR UPDATE; }
step s1c	{ COMMIT; }
teardown	{ ROLLBACK; }

session s2
step s2x	{ EXPLAIN (COSTS OFF) SELECT x.id FROM ifx x WHERE NOT coalesce(x.valid <@ (SELECT range_agg(r) FROM (SELECT y.valid r FROM ifr y WHERE y.id <= 13 FOR KEY SHARE OF y) y2), false) FOR KEY SHARE OF x; }
step s2c	{ SELECT x.id FROM ifx x WHERE NOT coalesce(x.valid <@ (SELECT range_agg(r) FROM (SELECT y.valid r FROM ifr y WHERE y.id <= 13 FOR KEY SHARE OF y) y2), false) FOR KEY SHARE OF x; }
step s2s	{ SELECT x.id FROM ifx x WHERE NOT coalesce(x.valid <@ (SELECT range_agg(r) FROM (SELECT y.valid r FROM ifr y WHERE y.id <= 13 FOR SHARE OF y) y2), false) FOR KEY SHARE OF x; }
step s2nw	{ SELECT x.id FROM ifx x WHERE NOT coalesce(x.valid <@ (SELECT range_agg(r) FROM (SELECT y.valid r FROM ifr y WHERE y.id <= 13 FOR KEY SHARE OF y NOWAIT) y2), false) FOR KEY SHARE OF x; }
step s2sk	{ SELECT x.id FROM ifx x WHERE NOT coalesce(x.valid <@ (SELECT range_agg(r) FROM (SELECT y.valid r FROM ifr y WHERE y.id <= 13 FOR KEY SHARE OF y SKIP LOCKED) y2), false) FOR KEY SHARE OF x; }

permutation s2x s1u s2c s1c
permutation s1u s2s s1c
permutation s1k s2c s1c
permutation s1kv s2c s1c
permutation s1d s2c s1c
permutation s1m s2c s1c
permutation s1x s2c s1c
permutation s1xd s2c s1c
permutation s1l s2nw s1c
permutation s1l s2sk s1c

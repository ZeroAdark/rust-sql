# D3373 (m8_drain_spool_write): an UPDATE .. FROM / DELETE .. USING whose FROM is a RIGHT JOIN, planned as ModifyTable
# over a Hash Right Join with the target in the build (a Hash Join of the target's scan and it on its key) and the
# nullable side probing. Gravity spools the probe's joined rows, then hands on the unmatched build records (drained,
# null-extended) and writes each target row once. Over a newer version of a target row (READ COMMITTED) PostgreSQL's
# EvalPlanQual reruns the plan over the target's newest version, dt's REFERENCE row and dc's REFERENCE row (none for a
# drained row): a joined row stays joined, a drained row stays drained; a row s1 deleted is skipped; a row s1 re-keyed
# (or moved onto the id of a row it deleted, 7 -> 8) no longer joins its dt row and is skipped. Expected output is
# PostgreSQL 18.6's.
setup
{
  CREATE TABLE dc (id int PRIMARY KEY, name text NOT NULL);
  INSERT INTO dc SELECT i, 'c' || i FROM generate_series(1, 4000) i;
  CREATE TABLE dt (id int PRIMARY KEY, cid int, pop int NOT NULL);
  INSERT INTO dt SELECT i, CASE WHEN i % 29 = 0 THEN NULL WHEN i % 3 = 0 THEN 100000 + i ELSE i * 11 END, i * 10 FROM generate_series(1, 60) i;
  CREATE TABLE du (id int PRIMARY KEY, pop int NOT NULL);
  INSERT INTO du SELECT id, pop FROM dt;
  ANALYZE dc, dt, du;
}
teardown { DROP TABLE dc, dt, du; }

session s1
setup		{ BEGIN; }
step s1u	{ UPDATE du SET pop = pop + 1000 WHERE id IN (1, 3, 29); }
step s1d	{ DELETE FROM du WHERE id IN (2, 6); }
step s1k	{ UPDATE du SET id = id + 500 WHERE id IN (4, 9); }
step s1m	{ UPDATE du SET pop = -1 WHERE id = 8; DELETE FROM du WHERE id = 8; UPDATE du SET id = 8 WHERE id = 7; }
step s1c	{ COMMIT; }

session s2
setup		{ SET enable_mergejoin = off; SET enable_nestloop = off; SET enable_memoize = off; SET max_parallel_workers_per_gather = 0; }
step s2u	{ UPDATE du u SET pop = u.pop + 1 + coalesce(length(c.name), 0) FROM dc c RIGHT JOIN dt t ON c.id = t.cid WHERE u.id = t.id AND t.id <= 30; }
step s2d	{ DELETE FROM du u USING dc c RIGHT JOIN dt t ON c.id = t.cid WHERE u.id = t.id AND t.id <= 30 AND (c.id IS NULL OR u.pop > 1000); }
step s2r	{ SELECT id, pop FROM du WHERE id <= 30 OR id > 500 ORDER BY id; }

permutation s1u s2u s1c s2r
permutation s1d s1k s2u s1c s2r
permutation s1m s2u s1c s2r
permutation s1u s2d s1c s2r
permutation s1d s1k s2d s1c s2r

# W5 multilock Row A (lowerv4 write/lock_marks.rs): NOWAIT per mark over a hash join, and each mark's own strength.
# - FOR UPDATE OF a, b NOWAIT with s1 holding b's row 1, then a's: 55P03 names each relation;
# - FOR KEY SHARE OF a NOWAIT FOR UPDATE OF b NOWAIT against s1's FOR NO KEY UPDATE of a's row 1 and FOR SHARE of b's:
#   KEY SHARE and NO KEY UPDATE do not conflict, UPDATE and SHARE do (55P03 on mb); FOR SHARE OF b instead does not.
# Expected output is the C oracle's (PostgreSQL 18, pgdg).

setup
{
  CREATE TABLE ma (id int PRIMARY KEY, v int NOT NULL);
  INSERT INTO ma SELECT i, i FROM generate_series(1, 10) i;
  CREATE TABLE mb (id int PRIMARY KEY, a_id int NOT NULL, w int NOT NULL);
  INSERT INTO mb SELECT i, i, i * 10 FROM generate_series(1, 10) i;
  ANALYZE ma, mb;
}

teardown
{
  DROP TABLE mb;
  DROP TABLE ma;
}

session s1
setup		{ BEGIN; }
step s1la	{ SELECT id FROM ma WHERE id = 1 FOR UPDATE; }
step s1lb	{ SELECT id FROM mb WHERE id = 1 FOR UPDATE; }
step s1mix	{ SELECT id FROM ma WHERE id = 1 FOR NO KEY UPDATE; SELECT id FROM mb WHERE id = 1 FOR SHARE; }
step s1c	{ COMMIT; }

session s2
setup		{ SET enable_nestloop = off; SET enable_mergejoin = off; }
step s2b	{ BEGIN; }
step s2x	{ EXPLAIN (COSTS OFF) SELECT a.id, b.id FROM ma a JOIN mb b ON b.a_id = a.id WHERE b.w < 30 FOR UPDATE OF a, b NOWAIT; }
step s2nw	{ SELECT a.id, b.id FROM ma a JOIN mb b ON b.a_id = a.id WHERE b.w < 30 FOR UPDATE OF a, b NOWAIT; }
step s2ksu	{ SELECT a.id, b.id FROM ma a JOIN mb b ON b.a_id = a.id WHERE b.w < 30 FOR KEY SHARE OF a NOWAIT FOR UPDATE OF b NOWAIT; }
step s2kss	{ SELECT a.id, b.id FROM ma a JOIN mb b ON b.a_id = a.id WHERE b.w < 30 FOR KEY SHARE OF a NOWAIT FOR SHARE OF b NOWAIT; }
step s2c	{ COMMIT; }

permutation s2b s2x s1lb s2nw s1c s2c
permutation s2b s1la s2nw s1c s2c
permutation s2b s1mix s2ksu s1c s2c
permutation s2b s1mix s2kss s1c s2c

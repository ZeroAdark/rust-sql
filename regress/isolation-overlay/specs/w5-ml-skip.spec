# W5 multilock Row A (lowerv4 write/lock_marks.rs): SKIP LOCKED per mark over a hash join. A mark that cannot be locked
# skips the whole row; the marks locked before it stay locked (ExecLockRows' goto lnext), the later ones are not tried.
# - FOR UPDATE SKIP LOCKED with s1 holding b's row 1: row 1 is skipped, and s1's NOWAIT on a's row 1 raises 55P03 (a's
#   lock taken before the skip is kept);
# - FOR UPDATE OF a SKIP LOCKED FOR UPDATE OF b: s1 holding a's row 1 skips the row; s1 holding b's row 1 makes s2 wait
#   (b's policy blocks).
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
step s1na	{ SELECT id FROM ma WHERE id = 1 FOR UPDATE NOWAIT; }
step s1c	{ COMMIT; }

session s2
setup		{ SET enable_nestloop = off; SET enable_mergejoin = off; }
step s2b	{ BEGIN; }
step s2x	{ EXPLAIN (COSTS OFF) SELECT a.id, b.id FROM ma a JOIN mb b ON b.a_id = a.id WHERE b.w < 40 FOR UPDATE SKIP LOCKED; }
step s2sk	{ SELECT a.id, b.id FROM ma a JOIN mb b ON b.a_id = a.id WHERE b.w < 40 FOR UPDATE SKIP LOCKED; }
step s2mx	{ SELECT a.id, b.id FROM ma a JOIN mb b ON b.a_id = a.id WHERE b.w < 40 FOR UPDATE OF a SKIP LOCKED FOR UPDATE OF b; }
step s2c	{ COMMIT; }

# b's row 1 held: row 1 skipped, a's row 1 kept locked
permutation s2b s2x s1lb s2sk s1na s1c s2c
# a's row 1 held under a's SKIP LOCKED: skipped
permutation s2b s1la s2mx s1c s2c
# b's row 1 held under b's default policy: s2 waits until s1 ends
permutation s2b s1lb s2mx s1c s2c

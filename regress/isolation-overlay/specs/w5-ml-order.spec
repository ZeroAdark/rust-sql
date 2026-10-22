# W5 multilock Row A (lowerv4 write/lock_marks.rs): ExecLockRows locks each row's marks in PlanRowMark order (the locking
# clauses in range table order), whichever side of the hash join scans the relation. s1 holds b's row 1:
# - s2's `ma a JOIN mb b .. FOR UPDATE` locks a's row 1, then waits on b's: s1's NOWAIT on a's row 1 raises 55P03;
# - s2's `mb b JOIN ma a .. FOR UPDATE` waits on b's row 1 before it locks a's: s1's NOWAIT on a's row 1 succeeds;
# - s1 then locking in the other order closes a cycle: s1 (the short deadlock_timeout) detects the deadlock, and s2
#   finishes after s1's rollback.
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
setup		{ BEGIN; SET deadlock_timeout = '10ms'; SET enable_nestloop = off; SET enable_mergejoin = off; }
step s1lb	{ SELECT id FROM mb WHERE id = 1 FOR UPDATE; }
step s1na	{ SELECT id FROM ma WHERE id = 1 FOR UPDATE NOWAIT; }
step s1ba	{ SELECT a.id, b.id FROM mb b JOIN ma a ON a.id = b.a_id WHERE b.w < 30 FOR UPDATE; }
step s1c	{ COMMIT; }

session s2
setup		{ SET deadlock_timeout = '100s'; SET enable_nestloop = off; SET enable_mergejoin = off; }
step s2b	{ BEGIN; }
step s2x	{ EXPLAIN (COSTS OFF) SELECT a.id, b.id FROM ma a JOIN mb b ON b.a_id = a.id WHERE b.w < 30 FOR UPDATE; }
step s2ab	{ SELECT a.id, b.id FROM ma a JOIN mb b ON b.a_id = a.id WHERE b.w < 30 FOR UPDATE; }
step s2xba	{ EXPLAIN (COSTS OFF) SELECT a.id, b.id FROM mb b JOIN ma a ON a.id = b.a_id WHERE b.w < 30 FOR UPDATE; }
step s2ba	{ SELECT a.id, b.id FROM mb b JOIN ma a ON a.id = b.a_id WHERE b.w < 30 FOR UPDATE; }
step s2c	{ COMMIT; }

# a first: a's row 1 is locked while s2 waits on b's
permutation s2b s2x s1lb s2ab s1na s1c s2c
# b first: s2 waits on b's row 1 holding nothing; a's row 1 is free
permutation s2b s2xba s1lb s2ba s1na s1c s2c
# opposite orders: s2 holds a's row 1 and waits on b's; s1 holds b's and waits on a's: deadlock detected (s1)
# D3352: two timing dependencies of the isolation tester, as PostgreSQL's own deadlock-hard.spec notes: the tester may or
# may not observe s1ba during its short lock wait (s1's 10 ms deadlock_timeout against the tester's 10 ms lock-wait poll;
# under load the deadlock error can arrive before the first poll, PostgreSQL's postgres too), and s1ba's error releases
# s1's locks (the aborted block's AbortTransaction), so s2ab's completion and s1ba's reach the tester in either order. The
# markers: s1ba(*) reports it waiting as soon as it is launched; s1ba(s2ab) reports its completion after s2ab's.
permutation s2b s1lb s2ab s1ba(*, s2ab) s1c s2c

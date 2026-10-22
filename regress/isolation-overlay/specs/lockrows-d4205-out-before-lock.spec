# D4205 (Phase 0c CE-4): PostgreSQL sends each row (printtup: the client encoding's conversion, then the socket) before
# ExecLockRows locks the next one. Under client_encoding LATIN1 a first row whose text has no LATIN1 form ('a-€') raises
# 22P05 at that row, before the second row's lock: s2 holds the second row, and its later lock on the first row goes
# through at once. A lock that ran ahead of the first row's send waits on the second row instead, and s2's lock on the
# first row deadlocks (40P01). Forms: lockrows 15b (Limit -> LockRows -> Sort -> Seq Scan) waiting and with NOWAIT, and a
# lock over an index scan in id order. Expected output is PostgreSQL 18.6's.
setup
{
  CREATE TABLE d4205 (id int PRIMARY KEY, k int NOT NULL, v text NOT NULL);
  INSERT INTO d4205 SELECT i, (i * 37) % 101, CASE WHEN i = 101 THEN 'a-€' ELSE 'v' || i END FROM generate_series(1, 400) i;
  ANALYZE d4205;
}
teardown { DROP TABLE d4205; }

session s1
setup		{ SET client_encoding = 'LATIN1'; }
step s1sort	{ SELECT id, k, v FROM d4205 WHERE k < 10 ORDER BY k, id LIMIT 3 FOR UPDATE; }
step s1nw	{ SELECT id, k, v FROM d4205 WHERE k < 10 ORDER BY k, id LIMIT 3 FOR UPDATE NOWAIT; }
step s1idx	{ SELECT id, v FROM d4205 WHERE id IN (101, 202, 303) ORDER BY id FOR UPDATE; }

session s2
step s2b	{ BEGIN; }
step s2r2	{ SELECT id FROM d4205 WHERE id = 202 FOR UPDATE; }
step s2r1	{ SELECT id FROM d4205 WHERE id = 101 FOR UPDATE; }
step s2c	{ COMMIT; }

# k < 10 by (k, id): 101 (0), 202 (0), 303 (0), ...
permutation s2b s2r2 s1sort s2r1 s2c
permutation s2b s2r2 s1nw s2r1 s2c
permutation s2b s2r2 s1idx s2r1 s2c

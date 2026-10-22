# LOCK (pgrc delta-store, 2026-10-03): NOWAIT on a pgrcolumnar table, PG's nowait, nowait-2/3/4 on a twin (row 1 in a
# sealed part, row 2 in the live segment); the error names the table, as heap's does. Expected output: the heap twin
# (scripts/pgrc-isolation.sh).

setup { CREATE TABLE foo (id int, data text NOT NULL) USING pgrcolumnar; }
setup { INSERT INTO foo VALUES (1, 'x'); }
setup { CALL pgrc_seal('foo'); }
setup { INSERT INTO foo VALUES (2, 'x'); }

teardown { DROP TABLE foo; }

session s1
step s1begin { BEGIN; }
step s1a     { SELECT * FROM foo ORDER BY id FOR UPDATE NOWAIT; }
step s1sh    { SELECT * FROM foo ORDER BY id FOR SHARE NOWAIT; }
step s1up    { SELECT * FROM foo ORDER BY id FOR UPDATE; }
step s1adv   { SELECT * FROM foo WHERE pg_advisory_lock(0) IS NOT NULL ORDER BY id FOR UPDATE NOWAIT; }
step s1c     { COMMIT; }

session s2
step s2begin { BEGIN; }
step s2a     { SELECT * FROM foo ORDER BY id FOR UPDATE NOWAIT; }
step s2sh    { SELECT * FROM foo ORDER BY id FOR SHARE NOWAIT; }
step s2up    { SELECT * FROM foo ORDER BY id FOR UPDATE; }
step s2del   { DELETE FROM foo WHERE id = 2; }
step s2lock  { SELECT pg_advisory_lock(0); }
step s2u     { UPDATE foo SET data = data; }
step s2unlock { SELECT pg_advisory_unlock(0); }
step s2c     { COMMIT; }

session s3
step s3begin { BEGIN; }
step s3a     { SELECT * FROM foo ORDER BY id FOR UPDATE NOWAIT; }
step s3c     { COMMIT; }

# nowait
permutation s1begin s2begin s1a s1c s2a s2c
permutation s1begin s2begin s1a s2a s1c s2c
permutation s1begin s2begin s2a s1a s1c s2c
# nowait-2: FOR SHARE NOWAIT shares; FOR UPDATE NOWAIT fails on a shared row
permutation s1begin s2begin s1sh s2sh s2a s1c s2c
permutation s1begin s2begin s2sh s2a s1sh s1c s2c
# nowait-3: a waiter queued ahead
permutation s1begin s2begin s3begin s1up s2up s3a s1c s2c s3c
# nowait-4: an update chain under a NOWAIT scan
permutation s2lock s1begin s1adv s2u s2begin s2u s2unlock s1c s2c
# an in-progress delete: NOWAIT fails
permutation s2begin s2del s1begin s1a s2c s1c

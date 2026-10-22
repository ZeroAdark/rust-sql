# LOCK (pgrc delta-store, 2026-10-03): SKIP LOCKED on a pgrcolumnar table, PG's skip-locked, skip-locked-2/3/4 on a
# twin: a queue whose row 1 is in a sealed part and row 2 in the live segment. Expected output: the heap twin
# (scripts/pgrc-isolation.sh).

setup { CREATE TABLE queue (id int, data text NOT NULL, status text NOT NULL) USING pgrcolumnar; }
setup { INSERT INTO queue VALUES (1, 'foo', 'NEW'); }
setup { CALL pgrc_seal('queue'); }
setup { INSERT INTO queue VALUES (2, 'bar', 'NEW'); }

teardown { DROP TABLE queue; }

session s1
step s1begin { BEGIN; }
step s1a     { SELECT * FROM queue ORDER BY id FOR UPDATE SKIP LOCKED LIMIT 1; }
step s1b     { SELECT * FROM queue ORDER BY id FOR UPDATE SKIP LOCKED LIMIT 1; }
step s1sh    { SELECT * FROM queue ORDER BY id FOR SHARE SKIP LOCKED LIMIT 1; }
step s1up    { SELECT * FROM queue ORDER BY id FOR UPDATE LIMIT 1; }
step s1adv   { SELECT * FROM queue WHERE pg_advisory_lock(0) IS NOT NULL ORDER BY id LIMIT 1 FOR UPDATE SKIP LOCKED; }
step s1c     { COMMIT; }

session s2
step s2begin { BEGIN; }
step s2a     { SELECT * FROM queue ORDER BY id FOR UPDATE SKIP LOCKED LIMIT 1; }
step s2b     { SELECT * FROM queue ORDER BY id FOR UPDATE SKIP LOCKED LIMIT 1; }
step s2sh    { SELECT * FROM queue ORDER BY id FOR SHARE SKIP LOCKED LIMIT 1; }
step s2up    { SELECT * FROM queue ORDER BY id FOR UPDATE LIMIT 1; }
step s2lock  { SELECT pg_advisory_lock(0); }
step s2u1    { UPDATE queue SET data = data WHERE id = 1; }
step s2unlock { SELECT pg_advisory_unlock(0); }
step s2c     { COMMIT; }

session s3
step s3begin { BEGIN; }
step s3a     { SELECT * FROM queue ORDER BY id FOR UPDATE SKIP LOCKED LIMIT 1; }
step s3c     { COMMIT; }

# skip-locked
permutation s1begin s2begin s1a s1b s1c s2a s2b s2c
permutation s1begin s2begin s1a s2a s1b s2b s1c s2c
permutation s1begin s2begin s1a s2a s1b s2b s2c s1c
permutation s1begin s2begin s1a s2a s2b s1b s1c s2c
permutation s1begin s2begin s2a s1a s1b s2b s1c s2c
permutation s1begin s2begin s2a s2b s1a s1b s2c s1c
# skip-locked-2: FOR SHARE SKIP LOCKED shares; FOR UPDATE SKIP LOCKED skips a shared row
permutation s1begin s2begin s1sh s2sh s2b s1c s2c
permutation s1begin s2begin s2sh s1sh s2b s1c s2c
# skip-locked-3: a waiter queued ahead does not make SKIP LOCKED wait
permutation s1begin s2begin s3begin s1up s2up s3a s1c s2c s3c
# skip-locked-4: an update chain under a SKIP LOCKED scan
permutation s2lock s1begin s1adv s2u1 s2begin s2u1 s2unlock s1c s2c

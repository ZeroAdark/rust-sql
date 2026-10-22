# LOCK (pgrc delta-store, 2026-10-03): row locks against UPDATE and DELETE on a pgrcolumnar table, as heap's. A
# locker first: FOR KEY SHARE lets a (no-key) UPDATE through and blocks a DELETE; FOR SHARE / UPDATE block both until
# the locker ends. A writer first: the locker waits, then (READ COMMITTED) locks the new version or skips the deleted
# row, or (REPEATABLE READ) fails 40001; an aborted writer leaves the row to the locker. Lock-only then COMMIT, then a
# write. PG's lock-update-delete (the pg_advisory dance) and lock-update-traversal follow. Rows: id 1 in a sealed part,
# id 11 in the live segment. Expected output: the heap twin (scripts/pgrc-isolation.sh).

setup { CREATE TABLE pgrc_t (id int, v int) USING pgrcolumnar; }
setup { INSERT INTO pgrc_t SELECT g, 0 FROM generate_series(1, 10) g; }
setup { CALL pgrc_seal('pgrc_t'); }
setup { INSERT INTO pgrc_t SELECT g, 0 FROM generate_series(11, 20) g; }

teardown { DROP TABLE pgrc_t; }

session s1
step s1b     { BEGIN; }
step s1rr    { BEGIN ISOLATION LEVEL REPEATABLE READ; SELECT count(*) FROM pgrc_t; }
step s1ksp   { SELECT id, v FROM pgrc_t WHERE id = 1 FOR KEY SHARE; }
step s1kss   { SELECT id, v FROM pgrc_t WHERE id = 11 FOR KEY SHARE; }
step s1shp   { SELECT id, v FROM pgrc_t WHERE id = 1 FOR SHARE; }
step s1shs   { SELECT id, v FROM pgrc_t WHERE id = 11 FOR SHARE; }
step s1upp   { SELECT id, v FROM pgrc_t WHERE id = 1 FOR UPDATE; }
step s1ups   { SELECT id, v FROM pgrc_t WHERE id = 11 FOR UPDATE; }
step s1nkp   { SELECT id, v FROM pgrc_t WHERE id = 1 FOR NO KEY UPDATE; }
step s1c     { COMMIT; }
step s1a     { ROLLBACK; }

session s2
step s2b     { BEGIN; }
step s2wp    { UPDATE pgrc_t SET v = v + 1 WHERE id = 1 RETURNING id, v; }
step s2ws    { UPDATE pgrc_t SET v = v + 1 WHERE id = 11 RETURNING id, v; }
step s2dp    { DELETE FROM pgrc_t WHERE id = 1 RETURNING id; }
step s2ds    { DELETE FROM pgrc_t WHERE id = 11 RETURNING id; }
step s2c     { COMMIT; }
step s2a     { ROLLBACK; }

session r
step rsel    { SELECT id, v FROM pgrc_t WHERE id IN (1, 11) ORDER BY id; }

# a locker first
permutation s1b s1ksp s2wp s1c rsel
permutation s1b s1kss s2ws s1c rsel
permutation s1b s1ksp s2dp s1c rsel
permutation s1b s1kss s2ds s1c rsel
permutation s1b s1ksp s2dp s1a rsel
permutation s1b s1shp s2wp s1c rsel
permutation s1b s1shs s2ds s1c rsel
permutation s1b s1upp s2wp s1c rsel
permutation s1b s1ups s2ws s1a rsel
permutation s1b s1nkp s2dp s1c rsel
# lock only, committed, then a write: the LOCK row stays and the write goes through it
permutation s1b s1upp s1c s2wp rsel
permutation s1b s1ups s1c s2ds rsel
# a writer first: the locker waits; READ COMMITTED locks the new version or skips a deleted row
permutation s2b s2wp s1b s1upp s2c s1c rsel
permutation s2b s2ws s1b s1shs s2c s1c rsel
permutation s2b s2dp s1b s1upp s2c s1c rsel
permutation s2b s2ds s1b s1ups s2c s1c rsel
permutation s2b s2wp s1b s1upp s2a s1c rsel
permutation s2b s2ws s1b s1ksp s2c s1c rsel
permutation s2b s2wp s1b s1ksp s2c s1c rsel
permutation s2b s2dp s1b s1ksp s2c s1c rsel
# REPEATABLE READ: a concurrent update or delete since the snapshot is 40001
permutation s1rr s2b s2wp s1upp s2c s1c rsel
permutation s1rr s2b s2ds s1ups s2c s1c rsel
permutation s1rr s2b s2wp s1upp s2a s1c rsel
# two writers after a lock: both wait for the locker, then for each other
permutation s1b s1upp s2b s2wp s1c s2c rsel

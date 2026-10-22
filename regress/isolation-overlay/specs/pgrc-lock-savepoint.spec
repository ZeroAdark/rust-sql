# LOCK-F3 (pgrc delta-store): an exclusive row lock (FOR UPDATE) is its LOCK row's unfrozen insert, and a write by
# the same subtransaction turns that row into its tombstone in place. Savepoints: a lock taken before a savepoint
# outlives a write rolled back to it (s2 still waits for s1); a lock taken after a savepoint goes with a rollback to it
# (s2 does not wait); a lock, then two writes of the row in later commands, then a rollback of the second. Rows: id 1
# in a sealed part, id 11 in the live segment. Expected output: the heap twin (scripts/pgrc-isolation.sh).

setup { CREATE TABLE pgrc_t (id int, v int) USING pgrcolumnar; }
setup { INSERT INTO pgrc_t SELECT g, 0 FROM generate_series(1, 10) g; }
setup { CALL pgrc_seal('pgrc_t'); }
setup { INSERT INTO pgrc_t SELECT g, 0 FROM generate_series(11, 20) g; }

teardown { DROP TABLE pgrc_t; }

session s1
step s1b    { BEGIN; }
step s1l    { SELECT id, v FROM pgrc_t WHERE id IN (1, 11) ORDER BY id FOR UPDATE; }
step s1sp   { SAVEPOINT a; }
step s1u    { UPDATE pgrc_t SET v = v + 1 WHERE id IN (1, 11); }
step s1d    { DELETE FROM pgrc_t WHERE id IN (1, 11); }
step s1rb   { ROLLBACK TO a; }
step s1c    { COMMIT; }
step s1a    { ROLLBACK; }

session s2
step s2u    { UPDATE pgrc_t SET v = v + 10 WHERE id IN (1, 11); }
step s2l    { SELECT id, v FROM pgrc_t WHERE id IN (1, 11) ORDER BY id FOR UPDATE NOWAIT; }

session r
step rsel   { SELECT id, v FROM pgrc_t WHERE id IN (1, 11) ORDER BY id; }

# locked before the savepoint, written after it, rolled back to it: still locked
permutation s1b s1l s1sp s1u s1rb s2u s1c rsel
permutation s1b s1l s1sp s1d s1rb s2l s1c rsel
# locked after the savepoint, rolled back to it: free
permutation s1b s1sp s1l s1rb s2u s1c rsel
permutation s1b s1sp s1l s1u s1rb s2l s1c rsel
# locked, written, written again in a later command (the first write's new version), the second rolled back
permutation s1b s1l s1u s1sp s1u s1rb s2u s1c rsel
permutation s1b s1l s1u s1sp s1d s1rb s1a s2u rsel
# locked, then deleted: the waiter sees the delete
permutation s1b s1l s1d s2u s1c rsel

# pgrc F2 L5 (docs/design/pgrc-delta-v2/F2.md): UPDATE || UPDATE on one row of a
# pgrcolumnar table. The table has one sealed part (ids 1-10) and one live
# segment (ids 11-20); each permutation targets id 1 (part) or id 11 (segment).
# Under READ COMMITTED the second writer waits, then follows the first's
# version (EPQ); under REPEATABLE READ and SERIALIZABLE it fails with 40001.
# Expected output: the heap twin (scripts/pgrc-isolation.sh).

setup { CREATE TABLE pgrc_t (id int, v int) USING pgrcolumnar; }
setup { INSERT INTO pgrc_t SELECT g, 0 FROM generate_series(1, 10) g; }
setup { CALL pgrc_seal('pgrc_t'); }
setup { INSERT INTO pgrc_t SELECT g, 0 FROM generate_series(11, 20) g; }

teardown { DROP TABLE pgrc_t; }

session s1
step s1b    { BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1p    { UPDATE pgrc_t SET v = v + 1 WHERE id = 1 RETURNING id, v; }
step s1s    { UPDATE pgrc_t SET v = v + 1 WHERE id = 11 RETURNING id, v; }
step s1c    { COMMIT; }
step s1a    { ROLLBACK; }

session s2
step s2rc   { BEGIN ISOLATION LEVEL READ COMMITTED; }
step s2rr   { BEGIN ISOLATION LEVEL REPEATABLE READ; SELECT count(*) FROM pgrc_t; }
step s2sz   { BEGIN ISOLATION LEVEL SERIALIZABLE; SELECT count(*) FROM pgrc_t; }
step s2p    { UPDATE pgrc_t SET v = v + 10 WHERE id = 1 RETURNING id, v; }
step s2s    { UPDATE pgrc_t SET v = v + 10 WHERE id = 11 RETURNING id, v; }
step s2c    { COMMIT; }

session r
step rsel   { SELECT id, v FROM pgrc_t WHERE id IN (1, 11) ORDER BY id; }
step rcount { SELECT count(*), sum(v) FROM pgrc_t; }

# READ COMMITTED: the waiter follows the committed first writer
permutation s1b s1p s2rc s2p s1c s2c rsel rcount
permutation s1b s1s s2rc s2s s1c s2c rsel rcount
# READ COMMITTED: the first writer aborts, the waiter acts on the original row
permutation s1b s1p s2rc s2p s1a s2c rsel rcount
permutation s1b s1s s2rc s2s s1a s2c rsel rcount
# REPEATABLE READ / SERIALIZABLE: the waiter's snapshot predates the commit
permutation s2rr s1b s1p s2p s1c s2c rsel rcount
permutation s2rr s1b s1s s2s s1c s2c rsel rcount
permutation s2sz s1b s1p s2p s1c s2c rsel rcount
permutation s2sz s1b s1s s2s s1c s2c rsel rcount
# REPEATABLE READ: the first writer aborts, the waiter goes on
permutation s2rr s1b s1s s2s s1a s2c rsel rcount

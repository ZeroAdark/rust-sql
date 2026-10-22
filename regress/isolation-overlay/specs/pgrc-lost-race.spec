# pgrc F2 L5 (docs/design/pgrc-delta-v2/F2.md): two writers of one row past
# step 1 at once. Each UPDATE's SET list calls pgrc_gate(k, v), which waits on
# advisory lock k (held by session c), so both statements have scanned the row
# and hold their new version unwritten when c lets one of them go. The loser
# waits for the winner, then (READ COMMITTED) follows its version or
# (REPEATABLE READ) fails with 40001; on pgrc it leaves one orphan tombstone and
# no visible new version. Targets: id 1 (sealed part) and id 11 (live segment).
# Expected output: the heap twin (scripts/pgrc-isolation.sh).

setup { CREATE TABLE pgrc_t (id int, v int) USING pgrcolumnar; }
setup { INSERT INTO pgrc_t SELECT g, 0 FROM generate_series(1, 10) g; }
setup { CALL pgrc_seal('pgrc_t'); }
setup { INSERT INTO pgrc_t SELECT g, 0 FROM generate_series(11, 20) g; }
setup
{
  CREATE FUNCTION pgrc_gate(k int, v int) RETURNS int LANGUAGE sql VOLATILE
    AS 'SELECT pg_advisory_xact_lock_shared(k); SELECT v';
}

teardown
{
  DROP TABLE pgrc_t;
  DROP FUNCTION pgrc_gate(int, int);
}

session c
step chold  { SELECT pg_advisory_lock(1), pg_advisory_lock(2); }
step cgo1   { SELECT pg_advisory_unlock(1); }
step cgo2   { SELECT pg_advisory_unlock(2); }

session s1
step s1rc   { BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1rr   { BEGIN ISOLATION LEVEL REPEATABLE READ; SELECT count(*) FROM pgrc_t; }
step s1p    { UPDATE pgrc_t SET v = pgrc_gate(1, v + 1) WHERE id = 1 RETURNING id, v; }
step s1s    { UPDATE pgrc_t SET v = pgrc_gate(1, v + 1) WHERE id = 11 RETURNING id, v; }
step s1c    { COMMIT; }

session s2
step s2rc   { BEGIN ISOLATION LEVEL READ COMMITTED; }
step s2p    { UPDATE pgrc_t SET v = pgrc_gate(2, v + 10) WHERE id = 1 RETURNING id, v; }
step s2s    { UPDATE pgrc_t SET v = pgrc_gate(2, v + 10) WHERE id = 11 RETURNING id, v; }
step s2c    { COMMIT; }

session r
step rsel   { SELECT id, v FROM pgrc_t WHERE id IN (1, 11) ORDER BY id; }
step rcount { SELECT count(*), sum(v) FROM pgrc_t; }

# s2 writes first, s1 (which scanned first) follows it
permutation chold s1rc s1p s2rc s2p cgo2 cgo1 s2c s1c rsel rcount
permutation chold s1rc s1s s2rc s2s cgo2 cgo1 s2c s1c rsel rcount
# s1 writes first, s2 follows it
permutation chold s1rc s1p s2rc s2p cgo1 cgo2 s1c s2c rsel rcount
permutation chold s1rc s1s s2rc s2s cgo1 cgo2 s1c s2c rsel rcount
# the loser is REPEATABLE READ: 40001, the winner's version stands alone
permutation chold s1rr s1p s2rc s2p cgo2 cgo1 s2c s1c rsel rcount
permutation chold s1rr s1s s2rc s2s cgo2 cgo1 s2c s1c rsel rcount

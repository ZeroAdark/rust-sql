# pgrc F2 L7 (docs/design/pgrc-delta-v2/F2.md L7; design §4.7 counterexample):
# a READ COMMITTED writer U whose statement snapshot predates tombstone T.
# U scans id 1 (sealed part) or id 11 (segment row) and parks in its SET list
# or WHERE clause on pgrc_gate (advisory lock 1, held by session c). Session
# s2 then updates or deletes the row and commits: T. A compile (pgrc_seal =
# pgrc_flush: rotate, seal, compile, reclaim) runs while U is parked. U's
# snapshot holds the horizon below T, so the compile must skip T (m1 checks
# that the tombstone heap still holds exactly one row). U then writes: its
# probe finds T, it follows T's successor (EPQ) and the row has exactly one
# successor; after T was a DELETE, U touches nothing. Under REPEATABLE READ U
# fails with 40001. A final compile folds everything (m0: no tombstone left).
# TOMB item 2: a compile retires the tombstone heap and folds a retired heap
# only whole, once every tombstone in it is below the horizon; a writer after
# the retirement probes both heaps and inserts into the new current one. The
# final m0 follows two flushes: the first folds the retired heap, the second
# resets it and retires + folds the current one.
# pgrc_tombs_in(t, lo, hi) is true when lo..hi tombstones are unfolded
# (pgrc_tombstones: the current heap and an unfolded retired one)
# and always true on the heap twin (the AM name is split so the twin's
# pgrcolumnar -> heap rewrite leaves it alone).
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
setup
{
  CREATE FUNCTION pgrc_tombs_in(t regclass, lo bigint, hi bigint) RETURNS bool LANGUAGE plpgsql AS $$
  DECLARE n bigint;
  BEGIN
    IF (SELECT a.amname FROM pg_class c JOIN pg_am a ON a.oid = c.relam WHERE c.oid = t) <> 'pgrc' || 'olumnar' THEN
      RETURN true;
    END IF;
    n := pgrc_tombstones(t);
    RETURN n BETWEEN lo AND hi;
  END $$;
}

teardown
{
  DROP TABLE pgrc_t;
  DROP FUNCTION pgrc_gate(int, int);
  DROP FUNCTION pgrc_tombs_in(regclass, bigint, bigint);
}

session c
step chold  { SELECT pg_advisory_lock(1); }
step cgo    { SELECT pg_advisory_unlock(1); }

session s1
step s1rc   { BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1rr   { BEGIN ISOLATION LEVEL REPEATABLE READ; }
step s1up   { UPDATE pgrc_t SET v = pgrc_gate(1, v + 1) WHERE id = 1 RETURNING id, v; }
step s1us   { UPDATE pgrc_t SET v = pgrc_gate(1, v + 1) WHERE id = 11 RETURNING id, v; }
step s1dp   { DELETE FROM pgrc_t WHERE id = 1 AND pgrc_gate(1, v) IS NOT NULL RETURNING id, v; }
step s1c    { COMMIT; }

session s2
step s2up   { UPDATE pgrc_t SET v = v + 10 WHERE id = 1 RETURNING id, v; }
step s2us   { UPDATE pgrc_t SET v = v + 10 WHERE id = 11 RETURNING id, v; }
step s2dp   { DELETE FROM pgrc_t WHERE id = 1 RETURNING id, v; }

session m
step mflush { CALL pgrc_seal('pgrc_t'); }
step m1     { SELECT pgrc_tombs_in('pgrc_t', 1, 1) AS t_kept; }
step m0     { SELECT pgrc_tombs_in('pgrc_t', 0, 0) AS all_folded; }

session r
step rsel   { SELECT id, v FROM pgrc_t WHERE id IN (1, 11) ORDER BY id, v; }
step rcount { SELECT count(*), sum(v) FROM pgrc_t; }

# T = an UPDATE of a part row: U follows the successor
permutation chold s1rc s1up s2up mflush m1 cgo s1c mflush mflush m0 rsel rcount
# T = an UPDATE of a segment row (the compile's seal makes it a part row under U)
permutation chold s1rc s1us s2us mflush m1 cgo s1c mflush mflush m0 rsel rcount
# T = a DELETE: U updates nothing
permutation chold s1rc s1up s2dp mflush m1 cgo s1c mflush mflush m0 rsel rcount
# U is a DELETE: it deletes the successor
permutation chold s1rc s1dp s2up mflush m1 cgo s1c mflush mflush m0 rsel rcount
# REPEATABLE READ: U fails with 40001 instead of following T
permutation chold s1rr s1up s2up mflush m1 cgo s1c mflush mflush m0 rsel rcount

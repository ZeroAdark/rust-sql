# pgrc F2 L7 (docs/design/pgrc-delta-v2/F2.md L7): compile (pgrc_seal =
# pgrc_flush: rotate, seal, compile, reclaim) against writers. The table has
# one sealed part (ids 1-10) and one live segment (ids 11-20). A compile skips
# an uncommitted writer's tombstone (it is the row lock), runs while a second
# writer waits on the first, ignores an aborted writer's, and folds committed
# ones; writers after a compile update and delete the successors (segment rows
# and DV-bearing parts) and conflict exactly as heap writers do.
# TOMB item 2: a compile retires the tombstone heap and folds a retired heap
# only whole, once every tombstone in it is below the horizon; a writer after
# the retirement probes both heaps and inserts into the new current one. The
# final m0 follows two flushes: the first folds the retired heap, the second
# resets it and retires + folds the current one.
# pgrc_tombs_in(t, lo, hi) is true when lo..hi tombstones are unfolded
# (pgrc_tombstones: the current heap and an unfolded retired one)
# visible to its snapshot (an uncommitted writer's are not, so the checks run
# after the commit that follows the compile)
# and always true on the heap twin (the AM name is split so the twin's
# pgrcolumnar -> heap rewrite leaves it alone).
# Expected output: the heap twin (scripts/pgrc-isolation.sh).

setup { CREATE TABLE pgrc_t (id int, v int) USING pgrcolumnar; }
setup { INSERT INTO pgrc_t SELECT g, 0 FROM generate_series(1, 10) g; }
setup { CALL pgrc_seal('pgrc_t'); }
setup { INSERT INTO pgrc_t SELECT g, 0 FROM generate_series(11, 20) g; }
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
  DROP FUNCTION pgrc_tombs_in(regclass, bigint, bigint);
}

session s1
step s1b    { BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1up   { UPDATE pgrc_t SET v = v + 1 WHERE id = 1 RETURNING id, v; }
step s1us   { UPDATE pgrc_t SET v = v + 1 WHERE id = 11 RETURNING id, v; }
step s1dp   { DELETE FROM pgrc_t WHERE id = 2 RETURNING id, v; }
step s1c    { COMMIT; }
step s1a    { ROLLBACK; }

session s2
step s2b    { BEGIN ISOLATION LEVEL READ COMMITTED; }
step s2rr   { BEGIN ISOLATION LEVEL REPEATABLE READ; SELECT count(*) FROM pgrc_t; }
step s2up   { UPDATE pgrc_t SET v = v + 10 WHERE id = 1 RETURNING id, v; }
step s2us   { UPDATE pgrc_t SET v = v + 10 WHERE id = 11 RETURNING id, v; }
step s2dp   { DELETE FROM pgrc_t WHERE id = 2 RETURNING id, v; }
step s2c    { COMMIT; }

session m
step mflush { CALL pgrc_seal('pgrc_t'); }
step m1     { SELECT pgrc_tombs_in('pgrc_t', 1, 1) AS one_kept; }
step m2     { SELECT pgrc_tombs_in('pgrc_t', 2, 2) AS two_kept; }
step m0     { SELECT pgrc_tombs_in('pgrc_t', 0, 0) AS all_folded; }

session r
step rsel   { SELECT id, v FROM pgrc_t WHERE id IN (1, 2, 11) ORDER BY id, v; }
step rcount { SELECT count(*), sum(v) FROM pgrc_t; }

# an uncommitted writer's tombstone survives the compile; after its commit the next compile folds it
permutation s1b s1up s1us mflush s1c m2 rsel mflush mflush m0 rsel rcount
# a compile while a second writer waits on the first; the waiter follows the first's successor
permutation s1b s1up s2b s2up mflush s1c s2c m2 mflush mflush m0 rsel rcount
permutation s1b s1us s2b s2us mflush s1c s2c mflush mflush m0 rsel rcount
# the waiter's first writer aborts across a compile; the waiter acts on the original row
permutation s1b s1up s2b s2up mflush s1a s2c m1 mflush mflush m0 rsel rcount
# an aborted writer: nothing to fold, nothing lost
permutation s1b s1up s1dp s1a mflush mflush m0 rsel rcount
# committed writes fold; later writers update and delete the successors and the DV-bearing part
permutation s1b s1up s1us s1dp s1c mflush m0 s2b s2up s2us s2dp s2c rsel mflush mflush m0 rsel rcount
# REPEATABLE READ: its snapshot keeps the first writer's tombstone through the compile; its write fails with 40001
permutation s2rr s1b s1up s1c mflush m1 s2up s2c mflush mflush m0 rsel rcount

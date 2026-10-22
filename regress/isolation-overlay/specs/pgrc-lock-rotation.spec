# LOCK (pgrc delta-store, 2026-10-03): row locks held across the table's maintenance. A lock is a LOCK row keyed by
# RowIdent, which survives a seal (a segment row moving into a part), a compaction (rows moving into a new part) and
# the tombstone heaps' switch; a fold of the retired heap waits for its lockers (it needs a ShareLock the lockers'
# RowExclusiveLock blocks), so a lock held across two flushes still blocks a writer, which then deletes the row through
# the retired heap's LOCK row. pgrc_lk_squash compacts (a pgrcolumnar table only; nothing on the heap twin), as
# pgrc_seal flushes. Rows: id 1 in a sealed part, id 11 in the live segment. Expected output: the heap twin
# (scripts/pgrc-isolation.sh).

setup { CREATE TABLE pgrc_t (id int, v int) USING pgrcolumnar; }
setup { INSERT INTO pgrc_t SELECT g, 0 FROM generate_series(1, 10) g; }
setup { CALL pgrc_seal('pgrc_t'); }
setup { INSERT INTO pgrc_t SELECT g, 0 FROM generate_series(11, 20) g; }
setup
{
  CREATE FUNCTION pgrc_lk_compact(regclass, text) RETURNS int LANGUAGE internal STRICT VOLATILE AS 'pgrc_compact';
  CREATE PROCEDURE pgrc_lk_squash(t regclass) LANGUAGE plpgsql AS $$
  BEGIN
    IF (SELECT a.amname FROM pg_class c JOIN pg_am a ON a.oid = c.relam WHERE c.oid = t) = 'pgrc' || 'olumnar' THEN
      PERFORM pgrc_flush(t);
      PERFORM pgrc_lk_compact(t, 'concat');
    END IF;
  END $$;
}

teardown
{
  DROP TABLE pgrc_t;
  DROP PROCEDURE pgrc_lk_squash(regclass);
  DROP FUNCTION pgrc_lk_compact(regclass, text);
}

session s1
step s1b    { BEGIN; }
step s1ls   { SELECT id, v FROM pgrc_t WHERE id = 11 FOR UPDATE; }
step s1lb   { SELECT id, v FROM pgrc_t WHERE id IN (1, 11) ORDER BY id FOR UPDATE; }
step s1sh   { SELECT id, v FROM pgrc_t WHERE id IN (1, 11) ORDER BY id FOR SHARE; }
step s1c    { COMMIT; }
step s1a    { ROLLBACK; }

session m
step mseal  { CALL pgrc_seal('pgrc_t'); }
step msquash { CALL pgrc_lk_squash('pgrc_t'); }

session s2
step s2ds   { DELETE FROM pgrc_t WHERE id = 11 RETURNING id; }
step s2ub   { UPDATE pgrc_t SET v = v + 1 WHERE id IN (1, 11) RETURNING id, v; }
step s2sh   { SELECT id, v FROM pgrc_t WHERE id IN (1, 11) ORDER BY id FOR SHARE; }

session r
step rsel   { SELECT id, v FROM pgrc_t WHERE id IN (1, 11) ORDER BY id; }
step rcount { SELECT count(*), sum(v) FROM pgrc_t; }

# a segment row locked, then sealed into a part: the lock holds
permutation s1b s1ls mseal s2ds s1c rsel rcount
# locks held across a seal and a compaction of every part
permutation s1b s1lb msquash s2ub s1c rsel rcount
permutation s1b s1lb msquash s2ub s1a rsel rcount
# two flushes (a tombstone-heap switch, then a fold that must wait for the locker): still locked
permutation s1b s1lb mseal mseal s2ub s1c mseal rsel rcount
permutation s1b s1ls mseal mseal s2ds s1c mseal rsel rcount
# shared locks across a compaction, a writer behind both
permutation s1b s1sh msquash s2sh s2ub s1c rsel rcount
# the same without maintenance: the writer waits for s1, the share holder still running in the multixact that s2's
# committed FOR SHARE made of s1's lock (a LOCK row's first locker is a multixact member every backend sees running)
permutation s1b s1sh s2sh s2ub s1c rsel rcount
# lock-only, committed, then maintenance, then a write
permutation s1b s1lb s1c mseal mseal msquash s2ub rsel rcount

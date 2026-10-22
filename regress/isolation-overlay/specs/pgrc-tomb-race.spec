# pgrc TOMB item 2 (docs/design/pgrc-delta-v2/changes.md, TOMB): a writer's tombstone insert racing a switch of the
# tombstone heaps. Writer a probes the current heap (slot 1: the setup leaves slot 1 current and slot 0 folded and
# reset) clear and pauses before its insert (injection point pgrc-tomb-before-insert, pgrc_front tombs.rs; detached
# while a waits, so no other writer stops there). A flush switches the heaps (wd's tombstones give the current heap
# blocks; a's RowExclusiveLock keeps it from folding). Writer b probes both heaps clear, inserts into the new current
# heap (slot 0) and commits. Woken, a's insert finds its heap no longer current (tomb_insert's recheck), withdraws
# it, retries in slot 0, meets b's tombstone and, READ COMMITTED, follows b's successor: ids 1 and 11 get one
# successor each, v = 2, as on heap. Without the recheck a's tombstone lands in the retired heap next to b's in the
# current one, and ids 1 and 11 each get two successors (the red build in TOMB.done).
# P2, the first switch (pgrc_n has slot 0 only): writer a pauses before its first insert; woken with the point still
# attached, it inserts (taking the heap's index lock) and pauses again before its second, past that row's probe. The
# flush makes slot 1 and switches to it. Woken, a retries in slot 1, a heap its statement has not seen, having taken no
# new lock since the flush: the lookup accepts the flush's invalidations (tombs::relid) and finds it. Without that,
# a's stale catalog snapshot misses slot 1 and the DELETE fails "has no tombstone relation 1" (pgrc-tomb-e2e
# compile/dml-threshold: a worker's switch mid-DELETE).
# Expected output: regress/isolation-overlay/expected/pgrc-tomb-race.out. Only pgrc pauses, so the heap twin's
# output differs in where aup is reported waiting and completed; the rows and every check are the twin's (TOMB.done).
# pgrc_tombs_in(t, lo, hi): lo..hi tombstones unfolded (pgrc_tombstones); always true on the heap twin. After a
# commits: wd's 2 in the retired heap, b's 2, and a's 3 (the successor its first attempt had written, killed when it
# lost to b, and its two updates of b's successors). The front worker may fold the retired heap before the check
# (the flush left that fold to the worker), so it takes 5 or 7. P2's takes 1 to 3: the setup's 1 and a's first in the
# retired heap (folded or not), a's second in the new current heap.

setup { CREATE EXTENSION injection_points; }
setup { CREATE TABLE pgrc_t (id int, v int) USING pgrcolumnar; }
setup { INSERT INTO pgrc_t SELECT g, 0 FROM generate_series(1, 10) g; }
setup { CALL pgrc_seal('pgrc_t'); }
setup { INSERT INTO pgrc_t SELECT g, 0 FROM generate_series(11, 20) g; }
setup { DELETE FROM pgrc_t WHERE id = 20; }
setup { CALL pgrc_seal('pgrc_t'); }
setup { CALL pgrc_seal('pgrc_t'); }
setup { CREATE TABLE pgrc_n (id int, v int) USING pgrcolumnar; }
setup { INSERT INTO pgrc_n SELECT g, 0 FROM generate_series(1, 10) g; }
setup { CALL pgrc_seal('pgrc_n'); }
setup { DELETE FROM pgrc_n WHERE id = 10; }
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
  DROP TABLE pgrc_n;
  DROP FUNCTION pgrc_tombs_in(regclass, bigint, bigint);
  DO $$ BEGIN PERFORM injection_points_detach('pgrc-tomb-before-insert'); EXCEPTION WHEN OTHERS THEN NULL; END $$;
  DROP EXTENSION injection_points;
}

session w
step wd     { DELETE FROM pgrc_t WHERE id IN (2, 12); }

session a
step aattach { DO $$ BEGIN PERFORM injection_points_attach('pgrc-tomb-before-insert', 'wait'); END $$; }
step aup    { UPDATE pgrc_t SET v = v + 1 WHERE id IN (1, 11); }
step an     { DELETE FROM pgrc_n WHERE id IN (2, 3); }

session b
step bup    { UPDATE pgrc_t SET v = v + 1 WHERE id IN (1, 11); }

session m
step mdetach { DO $$ BEGIN PERFORM injection_points_detach('pgrc-tomb-before-insert'); END $$; }
step mwake  { DO $$ BEGIN PERFORM injection_points_wakeup('pgrc-tomb-before-insert'); EXCEPTION WHEN OTHERS THEN NULL; END $$; }
step mflush { CALL pgrc_seal('pgrc_t'); }
step m0     { SELECT pgrc_tombs_in('pgrc_t', 0, 0) AS none_unfolded; }
step m4     { SELECT pgrc_tombs_in('pgrc_t', 4, 4) AS four_unfolded; }
step m57    { SELECT pgrc_tombs_in('pgrc_t', 5, 7) AS five_to_seven_unfolded; }
step mflushn { CALL pgrc_seal('pgrc_n'); }
step mn13   { SELECT pgrc_tombs_in('pgrc_n', 1, 3) AS one_to_three_unfolded; }

session r
step rsel   { SELECT id, v FROM pgrc_t ORDER BY id, v; }
step rseln  { SELECT id FROM pgrc_n ORDER BY id; }

permutation wd aattach aup mdetach mflush bup m4 mwake m57 rsel mflush mflush m0 rsel
permutation aattach an mwake mdetach mflushn mwake mn13 rseln

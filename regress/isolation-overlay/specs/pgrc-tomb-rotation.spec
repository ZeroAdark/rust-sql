# pgrc TOMB item 2 (docs/design/pgrc-delta-v2/changes.md, TOMB): tombstone-heap rotation against readers and
# prepared writers. A compile (pgrc_seal = pgrc_flush: rotate, seal, compile, reclaim) retires the current
# tombstone heap (switch), folds a retired heap whole into the parts' DVs once every tombstone in it is below the
# horizon, and resets a folded heap to empty storage once the fold is below the horizon; no tombstone is ever
# deleted. The setup leaves slot 1 current and slot 0 folded and reset (one deleted row, id 20, two flushes), so the
# permutations switch back to slot 0.
# - An RR reader whose view predates the switch keeps reading the retired heap; one whose view predates the fold
#   reads it too (the row executor applies the retired heap's tombstones), and the heap's reset waits for that
#   reader's snapshot: the tombstone heaps keep blocks (front-bearing, Gravity D840) until the reset.
# - A prepared writer's tombstones in the retired heap are a row lock across the switch: a writer after the switch
#   probes both heaps and waits for it (lock_timeout: the isolation tester does not see a wait on a prepared
#   transaction, whose lock has no backend), and the fold waits for COMMIT / ROLLBACK PREPARED.
# The switch race of a writer is pgrc-tomb-race.
# pgrc_tombs_in(t, lo, hi): lo..hi tombstones unfolded (pgrc_tombstones, the current heap and an unfolded retired
# one, under the statement's snapshot). pgrc_tomb_blocks_in(t, lo, hi): lo..hi blocks in the two tombstone heaps
# together. Both are always true on the heap twin (the AM name is split so the twin's pgrcolumnar -> heap rewrite
# leaves it alone). PREPARE TRANSACTION needs max_prepared_transactions (scripts/pgrc-isolation.sh sets it).
# Expected output: the heap twin (scripts/pgrc-isolation.sh).
# Every flush first waits out other holders of the horizon (RESID item 11; q023g run 1 m0 t->f, D820 10-03 m4 t->f,
# both under load): a fold needs every tombstone's inserter below the horizon and a reset the fold's, so a transaction
# of another backend of the database that began before the last write or fold (a front-worker step it woke, an
# autoanalyze) leaves the heap unfolded or unreset through the flush, and the later counts see the backlog.
# pgrc_quiet() waits, at most 60 s, until no backend of this database outside the isolation tester's own sessions
# (application_name isolation/...: an open RR reader or a prepared writer still holds the fold, as the permutations
# intend) has an xid or xmin below the xmax it starts at, then fails naming the holder. It prints nothing.

setup
{
  CREATE PROCEDURE pgrc_quiet() LANGUAGE plpgsql AS $$
  DECLARE x bigint := pg_snapshot_xmax(pg_current_snapshot())::text::bigint; t0 timestamptz := clock_timestamp(); held text;
  BEGIN
    LOOP
      PERFORM pg_stat_clear_snapshot();
      SELECT string_agg(format('%s %s pid %s xid %s xmin %s', backend_type, application_name, pid, backend_xid, backend_xmin), '; ') INTO held
        FROM pg_stat_activity
       WHERE datname = current_database() AND pid <> pg_backend_pid()
         AND coalesce(application_name, '') NOT LIKE 'isolation/%'
         AND (backend_xid::text::bigint < x OR backend_xmin::text::bigint < x);
      EXIT WHEN held IS NULL;
      IF clock_timestamp() - t0 > interval '60 s' THEN
        RAISE EXCEPTION 'pgrc_quiet: horizon below xid % held for 60 s by %', x, held;
      END IF;
      -- pgrust: bound-only (poll interval of EXIT WHEN held IS NULL; the 60 s clock check bounds it and fails by name)
      PERFORM pg_sleep(0.01);
    END LOOP;
  END $$;
}

setup { CREATE TABLE pgrc_t (id int, v int) USING pgrcolumnar; }
setup { INSERT INTO pgrc_t SELECT g, 0 FROM generate_series(1, 10) g; }
setup { CALL pgrc_quiet(); CALL pgrc_seal('pgrc_t'); }
setup { INSERT INTO pgrc_t SELECT g, 0 FROM generate_series(11, 20) g; }
setup { DELETE FROM pgrc_t WHERE id = 20; }
setup { CALL pgrc_quiet(); CALL pgrc_seal('pgrc_t'); }
setup { CALL pgrc_quiet(); CALL pgrc_seal('pgrc_t'); }
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
setup
{
  CREATE FUNCTION pgrc_tomb_blocks_in(t regclass, lo bigint, hi bigint) RETURNS bool LANGUAGE plpgsql AS $$
  DECLARE n bigint := 0; h regclass;
  BEGIN
    IF (SELECT a.amname FROM pg_class c JOIN pg_am a ON a.oid = c.relam WHERE c.oid = t) <> 'pgrc' || 'olumnar' THEN
      RETURN true;
    END IF;
    FOREACH h IN ARRAY ARRAY[to_regclass(format('pg_toast.pgrc_tomb_%s', t::oid)),
                             to_regclass(format('pg_toast.pgrc_tomb_%s_1', t::oid))] LOOP
      IF h IS NOT NULL THEN
        n := n + pg_relation_size(h) / 8192;
      END IF;
    END LOOP;
    RETURN n BETWEEN lo AND hi;
  END $$;
}

teardown
{
  DROP TABLE pgrc_t;
  DROP FUNCTION pgrc_tombs_in(regclass, bigint, bigint);
  DROP FUNCTION pgrc_tomb_blocks_in(regclass, bigint, bigint);
  DROP PROCEDURE pgrc_quiet();
}

session s1
step s1rr   { BEGIN ISOLATION LEVEL REPEATABLE READ; }
step s1sel  { SELECT id, v FROM pgrc_t ORDER BY id, v; }
step s1c    { COMMIT; }

session s2
step s2rr   { BEGIN ISOLATION LEVEL REPEATABLE READ; }
step s2sel  { SELECT id, v FROM pgrc_t ORDER BY id, v; }
step s2c    { COMMIT; }

session w
# pgrust: hold (prepared transaction pgrc_tomb_p's row lock on ids 5 / 15, held until mcp / mrp, which run only after wu5 returns: the timeout always fires inside it)
setup       { SET lock_timeout = '500ms'; }
step wd     { DELETE FROM pgrc_t WHERE id IN (2, 12); }
step wu     { UPDATE pgrc_t SET v = v + 1 WHERE id IN (3, 13); }
step wu5    { UPDATE pgrc_t SET v = v + 10 WHERE id IN (5, 15) RETURNING id, v; }

session p
step pb     { BEGIN; }
step pu     { UPDATE pgrc_t SET v = v + 100 WHERE id IN (5, 15); }
step pp     { PREPARE TRANSACTION 'pgrc_tomb_p'; }

session m
step mflush { CALL pgrc_quiet(); CALL pgrc_seal('pgrc_t'); }
step mcp    { COMMIT PREPARED 'pgrc_tomb_p'; }
step mrp    { ROLLBACK PREPARED 'pgrc_tomb_p'; }
step m0     { SELECT pgrc_tombs_in('pgrc_t', 0, 0) AS none_unfolded; }
step m4     { SELECT pgrc_tombs_in('pgrc_t', 4, 4) AS four_unfolded; }
step mkept  { SELECT pgrc_tomb_blocks_in('pgrc_t', 1, 1000000) AS blocks_kept; }
step mreset { SELECT pgrc_tomb_blocks_in('pgrc_t', 0, 0) AS all_reset; }

session r
step rsel   { SELECT id, v FROM pgrc_t ORDER BY id, v; }

# RR readers across a switch (s1 predates the writes, so the fold waits for it), a fold (s2 predates it) and a reset
# (it waits for s2, the heap keeps its blocks until then)
permutation s1rr s1sel wd wu mflush m4 s1sel s2rr s2sel mflush s1c mflush m0 mkept s2sel mflush mkept s2sel s2c mflush mreset rsel
# a prepared writer across a switch: the flushes neither fold its heap nor let the later writer past its row lock;
# after COMMIT PREPARED the writer follows its successors, and its tombstones fold only after it
permutation pb pu pp mflush mflush wu5 mcp wu5 m4 mflush mflush m0 rsel
# ROLLBACK PREPARED: the later writer updates the original rows
permutation pb pu pp mflush wu5 mrp wu5 mflush mflush m0 rsel

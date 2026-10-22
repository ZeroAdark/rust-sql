# pgrc F2 L7 (docs/design/pgrc-delta-v2/F2.md L7): a REPEATABLE READ (or
# SERIALIZABLE, read-only) reader across compiles (pgrc_seal = pgrc_flush:
# rotate, seal, compile, reclaim) sees the same rows before and after. The
# table has one sealed part (ids 1-10) and one live segment (ids 11-20); the
# setup deletes ids 3 and 13 and updates ids 4 and 14, so four tombstones exist
# before any reader starts (two over the part, two over the segment). A
# compile after the reader's snapshot folds those (they are below its horizon)
# into the part's DV; writes after the reader's snapshot wait for the reader.
# The reader's rows never change.
# TOMB item 2: a compile retires the tombstone heap and folds it whole (switch
# + fold), or, with a tombstone above the horizon in it, only retires it (all
# of its tombstones stay unfolded until a later compile folds the heap); the
# reader keeps reading the retired heap (its view predates the fold), and the
# heap's reset waits for the reader's snapshot.
# pgrc_tombs_in(t, lo, hi) is true when lo..hi tombstones are unfolded
# (pgrc_tombstones: the current heap and an unfolded retired one) and always
# true on the heap twin (the AM name is split so the twin's
# pgrcolumnar -> heap rewrite leaves it alone).
# PGRCSERVE M0: the setup's rows are COPYs, so the spec runs before pgrc INSERT
# (M1): a plain COPY for the part, and for the segment a COPY whose WHERE is
# volatile (copyfrom.c CIM_SINGLE: rows into the front's segment, as the INSERT
# did). The setup's UPDATE and DELETE run through pgrc_m3, which prints `done`
# (PGRCSERVE M3 serves them; before it, the refusal by name, no OID).
# Expected output: regress/isolation-overlay/expected/pgrc-compile-rr.out
# (PGRCSERVE M3: regenerated from bin-int-r13h's served run). The twin's output,
# SERIALIZABLE permutation included, is byte-identical to the same spec on real
# PostgreSQL 18 (checked by F2 L7d; a read-only reader is clear of D551).

setup { CREATE TABLE pgrc_t (id int, v int) USING pgrcolumnar; }
setup { COPY pgrc_t FROM PROGRAM 'seq -f %g,0 1 10' WITH (FORMAT csv); }
setup { CALL pgrc_seal('pgrc_t'); }
setup { COPY pgrc_t FROM PROGRAM 'seq -f %g,0 11 20' WITH (FORMAT csv) WHERE random() >= 0; }
setup
{
  CREATE FUNCTION pgrc_m3(q text) RETURNS text LANGUAGE plpgsql AS $$
  BEGIN
    EXECUTE q;
    RETURN 'done';
  EXCEPTION WHEN feature_not_supported THEN
    RETURN 'refused until M3: ' || regexp_replace(SQLERRM, '\[\d+ relation\]', '[relation]');
  END $$;
}
setup { SELECT pgrc_m3('DELETE FROM pgrc_t WHERE id IN (3, 13)') AS setup_delete; }
setup { SELECT pgrc_m3('UPDATE pgrc_t SET v = 5 WHERE id IN (4, 14)') AS setup_update; }
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
  DROP FUNCTION pgrc_m3(text);
}

session s1
step s1rr   { BEGIN ISOLATION LEVEL REPEATABLE READ; }
step s1sz   { BEGIN ISOLATION LEVEL SERIALIZABLE READ ONLY; }
step s1sel  { SELECT id, v FROM pgrc_t ORDER BY id, v; }
step s1agg  { SELECT count(*), sum(v), min(id), max(id) FROM pgrc_t; }
step s1c    { COMMIT; }

session w
step wu     { UPDATE pgrc_t SET v = v + 1 WHERE id IN (1, 11); }
step wd     { DELETE FROM pgrc_t WHERE id IN (2, 12); }

session m
step mflush { CALL pgrc_seal('pgrc_t'); }
step m0     { SELECT pgrc_tombs_in('pgrc_t', 0, 0) AS all_folded; }
step m8     { SELECT pgrc_tombs_in('pgrc_t', 8, 8) AS eight_kept; }

session r
step rsel   { SELECT id, v FROM pgrc_t ORDER BY id, v; }

# the setup's tombstones fold under the reader
permutation s1rr s1sel s1agg mflush m0 s1sel s1agg s1c rsel
# writes after the reader's snapshot: the compile keeps them (and, TOMB, the setup's four in the same retired heap),
# the reader still sees its rows; after it ends they fold
permutation s1rr s1sel wu wd mflush m8 s1sel s1agg mflush s1sel s1c mflush m0 rsel
# the writes commit before the reader's snapshot and fold under it
permutation wu wd s1rr s1agg mflush m0 s1sel s1agg s1c rsel
# SERIALIZABLE READ ONLY reader
permutation s1sz s1sel wu wd mflush m8 s1sel s1agg s1c mflush m0 rsel

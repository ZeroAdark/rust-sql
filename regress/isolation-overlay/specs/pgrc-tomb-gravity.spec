# pgrc TOMB item 2 (Gravity 6e): tombstone-heap rotation against Gravity, which reads a pgrcolumnar table's parts
# only and so serves a statement only while the table is not front-bearing (pgrc_front::front_bearing: no front
# rows, no block in either tombstone heap). Meaningful with Gravity on (PGRUST_GRAVITY_SLOT unset); with Gravity off
# the spec passes as its twin. The setup leaves slot 1 current and slot 0 folded and reset (one deleted row, id 100,
# two flushes): a clear table.
# 1. A served REPEATABLE READ scan admitted clear pauses right after its admission (injection point
#    pgrc-front-served-clear, pgrc_front::front_bearing_relid_as; the transaction snapshot is the planner's, taken
#    before). While it waits, a DELETE puts tombstones in the current heap and two flushes retire that heap (the fold
#    waits for the scan's snapshot). Woken, the scan answers its snapshot's rows. With Gravity off the same scan
#    pauses at the row executor's scan start instead (pgrc-front-scan-start, filter::build, past its snapshot), so
#    the output is the same either way.
# 2. A REPEATABLE READ reader whose snapshot predates the fold of a retired heap: the heap's reset waits for its
#    snapshot, so the table stays front-bearing and the reader's later statements take the rows road (PGRCSERVE M0;
#    the row executor before), which applies the retired heap's tombstones. Reset early (a build without the reset's
#    horizon check), Gravity would serve the reader from its view's parts without the fold's DVs and count ids 7 and 8
#    again.
# The points are detached as soon as s1 waits (mdetach), so no other statement stops there; s2's DELETE runs in a DO
# block, through SPI. The wakeup of the point no one waits at is quiet.
# PGRCSERVE M0: the setup's rows are a COPY, so the spec runs before pgrc INSERT (M1). The setup's DELETE runs
# through pgrc_m3, which prints `done` (PGRCSERVE M3 serves it; before it, the refusal by name, no OID).
# Expected output: regress/isolation-overlay/expected/pgrc-tomb-gravity.out. Only pgrc pauses, so the heap twin's
# output differs in where s1agg is reported waiting and completed; the rows and every check are the twin's
# (TOMB.done has the diff). PGRCSERVE M3: regenerated from bin-int-r13h's served run.
# pgrc_tomb_blocks_in(t, lo, hi): lo..hi blocks in the two tombstone heaps together; always true on the heap twin.

setup { CREATE EXTENSION injection_points; }
setup { CREATE TABLE pgrc_g (id int, v int) USING pgrcolumnar; }
setup { COPY pgrc_g FROM PROGRAM 'seq 1 100 | sed p | paste -d, - -' WITH (FORMAT csv); }
setup { CALL pgrc_seal('pgrc_g'); }
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
setup { SELECT pgrc_m3('DELETE FROM pgrc_g WHERE id = 100') AS setup_delete; }
setup { CALL pgrc_seal('pgrc_g'); }
setup { CALL pgrc_seal('pgrc_g'); }
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
  DROP TABLE pgrc_g;
  DROP FUNCTION pgrc_tomb_blocks_in(regclass, bigint, bigint);
  DROP FUNCTION pgrc_m3(text);
  DO $$ BEGIN PERFORM injection_points_detach('pgrc-front-served-clear'); EXCEPTION WHEN OTHERS THEN NULL; END $$;
  DO $$ BEGIN PERFORM injection_points_detach('pgrc-front-scan-start'); EXCEPTION WHEN OTHERS THEN NULL; END $$;
  DROP EXTENSION injection_points;
}

session s1
step s1wait { DO $$ BEGIN PERFORM injection_points_attach('pgrc-front-served-clear', 'wait'); PERFORM injection_points_attach('pgrc-front-scan-start', 'wait'); END $$; }
step s1rr   { BEGIN ISOLATION LEVEL REPEATABLE READ; }
step s1agg  { SELECT count(*), sum(v) FROM pgrc_g; }
step s1c    { COMMIT; }

session s2
step s2del  { DO $$ BEGIN DELETE FROM pgrc_g WHERE id IN (7, 8); END $$; }
step s2wake { DO $$ BEGIN PERFORM injection_points_wakeup('pgrc-front-served-clear'); EXCEPTION WHEN OTHERS THEN NULL; END $$;
              DO $$ BEGIN PERFORM injection_points_wakeup('pgrc-front-scan-start'); EXCEPTION WHEN OTHERS THEN NULL; END $$; }

session m
step mdetach { DO $$ BEGIN PERFORM injection_points_detach('pgrc-front-served-clear'); PERFORM injection_points_detach('pgrc-front-scan-start'); END $$; }
step mflush { CALL pgrc_seal('pgrc_g'); }
step mbear  { SELECT pgrc_tomb_blocks_in('pgrc_g', 1, 1000000) AS tomb_blocks; }
step mclear { SELECT pgrc_tomb_blocks_in('pgrc_g', 0, 0) AS no_tomb_blocks; }

session r
step ragg   { SELECT count(*), sum(v) FROM pgrc_g; }

# 1: a served scan across a DELETE into the current heap and its retirement
permutation s1wait s1rr s1agg mdetach s2del mbear mflush mflush s2wake s1c mflush mflush mclear ragg
# 2: the reset waits for a snapshot older than the fold; the reader declines until then
permutation s2del s1rr s1agg mflush mbear mflush mbear s1agg s1c mflush mclear ragg

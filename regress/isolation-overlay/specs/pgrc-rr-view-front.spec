# pgrc RR-VIEW (pgrc-plans/RR-VIEW-design.md): the reader's own view over a table with front rows and tombstones.
# Before s1's snapshot the table holds a part (ids 1-10), front rows (ids 11-15) and two committed tombstones (id 3 on
# the part, id 13 in the front). After it, s2 inserts front rows and deletes rows (tombstones s1 does not see), and m's
# flush seals the front into a part, folds the horizon-passed tombstones into a DV and rotates the ring. s1's own bulk
# part then chains on that generation: s1 must still read the pre-snapshot front rows from their segment under its
# snapshot (the sealed part also holds s2's rows), apply only the tombstones its snapshot sees, and see its own rows,
# a front row it inserts included (before the flush, in a segment it holds, or after, in the one the flush activated).
# PGRCSERVE M0: the writes are COPYs, so the spec holds before pgrc INSERT (M1). A plain COPY loads the bulk lane (a
# part publish at statement end); a COPY whose WHERE is volatile (copyfrom.c CIM_SINGLE) puts its rows in the front's
# segment (front rows), as a short INSERT did. The heap twin runs the same COPYs. The setup's DELETE runs
# through pgrc_m3, which prints `done` (PGRCSERVE M3 serves it; before it, the refusal by name, no OID).
# Expected output: regress/isolation-overlay/expected/pgrc-rr-view-front.out (PGRCSERVE M3: regenerated
# from bin-int-r13h's served run).

setup { CREATE TABLE pgrc_t (id int, v int) USING pgrcolumnar; }
setup { COPY pgrc_t FROM PROGRAM 'seq -f %g,0 1 10' WITH (FORMAT csv); }
setup { CALL pgrc_seal('pgrc_t'); }
setup { COPY pgrc_t FROM PROGRAM 'seq -f %g,0 11 15' WITH (FORMAT csv) WHERE random() >= 0; }
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

teardown { DROP TABLE pgrc_t; DROP FUNCTION pgrc_m3(text); }

session s1
step s1rr   { BEGIN ISOLATION LEVEL REPEATABLE READ; }
step s1ser  { BEGIN ISOLATION LEVEL SERIALIZABLE; }
step s1rc   { BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1agg  { SELECT count(*), sum(id), min(id), max(id), sum(v) FROM pgrc_t; }
step s1some { SELECT id, v FROM pgrc_t WHERE id < 20 OR id > 3000 ORDER BY id; }
step s1bulk { COPY pgrc_t FROM PROGRAM 'seq -f %g,1 1001 1040' WITH (FORMAT csv); }
step s1row  { COPY pgrc_t FROM PROGRAM 'echo 3001,1' WITH (FORMAT csv) WHERE random() >= 0; }
step s1c    { COMMIT; }

session s2
step s2rows { COPY pgrc_t FROM PROGRAM 'seq -f %g,2 601 605' WITH (FORMAT csv) WHERE random() >= 0; }
step s2del  { DELETE FROM pgrc_t WHERE id IN (1, 11, 602); }

session m
step mseal  { CALL pgrc_seal('pgrc_t'); }

session r
step ragg   { SELECT count(*), sum(id), min(id), max(id), sum(v) FROM pgrc_t; }

# pre-snapshot front rows sealed with rows and tombstones committed after the snapshot, then the reader's bulk part
permutation s1rr s1agg s2rows s2del mseal s1bulk s1agg s1some s1c ragg
# ... and the reader's own front row, in the segment the flush activated
permutation s1rr s1agg s2rows mseal s1bulk s1row s1agg s1some s1c ragg
# the reader's own front row before the flush: it holds that segment, which the flush leaves live
permutation s1rr s1row s1agg s2rows s2del mseal s1bulk s1agg s1some s1c ragg
# SERIALIZABLE
permutation s1ser s1agg s2rows s2del mseal s1bulk s1row s1agg s1some s1c ragg
# READ COMMITTED: each statement sees the latest commits
permutation s1rc s1agg s2rows s2del mseal s1bulk s1row s1agg s1some s1c ragg

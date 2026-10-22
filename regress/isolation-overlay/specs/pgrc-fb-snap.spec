# pgrc FB-SNAP: Gravity's road for a front-bearing table (the rows road since PGRCSERVE M0, F1-3's decline before) is
# decided under the statement's snapshot. s1's read of a
# sealed table without tombstones pauses in Gravity's simple-protocol entry just before its snapshot (injection point
# gravity-simple-before-snapshot); s2 commits the table's first tombstone (DELETE) or first front row (COPY) and
# wakes s1. s1's snapshot sees that row, so a read of the parts alone counts the deleted row or misses the new one.
# Only Gravity pauses there: run with PGRUST_GRAVITY_SLOT unset for the check; with Gravity off the spec passes as its
# twin. The attach, s2's writes and the wakeup run in DO blocks, which Gravity's simple entry never serves.
# PGRCSERVE M0: the rows are COPYs, so the spec holds before pgrc INSERT (M1): the setup's plain COPY loads the bulk lane,
# and s2row's COPY, whose WHERE is volatile (copyfrom.c CIM_SINGLE), puts its row in the front's segment, as the INSERT
# did. The heap twin runs the same COPYs. pgrc DELETE is M3 (served since): before it s2del was refused and committed no
# tombstone.
# Expected output: regress/isolation-overlay/expected/pgrc-fb-snap.out (PGRCSERVE M3: regenerated
# from bin-int-r13h's served run).

setup { CREATE EXTENSION injection_points; }
setup { CREATE TABLE pgrc_fbs (id int, v int) USING pgrcolumnar; }
setup { COPY pgrc_fbs FROM PROGRAM 'seq 1 100 | sed p | paste -d, - -' WITH (FORMAT csv); }
setup { CALL pgrc_seal('pgrc_fbs'); }

teardown
{
    DROP TABLE pgrc_fbs;
    DO $$ BEGIN PERFORM injection_points_detach('gravity-simple-before-snapshot'); EXCEPTION WHEN OTHERS THEN NULL; END $$;
    DROP EXTENSION injection_points;
}

session s1
step s1wait { DO $$ BEGIN PERFORM injection_points_attach('gravity-simple-before-snapshot', 'wait'); END $$; }
step s1rr   { BEGIN ISOLATION LEVEL REPEATABLE READ; }
step s1agg  { SELECT count(*), sum(v) FROM pgrc_fbs; }
step s1c    { COMMIT; }

session s2
step s2del  { DO $$ BEGIN DELETE FROM pgrc_fbs WHERE id = 7; END $$; }
step s2row  { DO $$ BEGIN COPY pgrc_fbs FROM PROGRAM 'echo 101,101' WITH (FORMAT csv) WHERE random() >= 0; END $$; }
step s2wake { DO $$ BEGIN PERFORM injection_points_detach('gravity-simple-before-snapshot'); PERFORM injection_points_wakeup('gravity-simple-before-snapshot'); END $$; }

session r
step ragg   { SELECT count(*), sum(v) FROM pgrc_fbs; }

# the first tombstone, committed while s1 waits: s1 must not count id 7
permutation s1wait s1agg s2del s2wake ragg
# the first front row: s1 must count id 101
permutation s1wait s1agg s2row s2wake ragg
# REPEATABLE READ: the transaction's snapshot is the planner's, taken before the wait, so s1 does not see the delete
permutation s1wait s1rr s1agg s2del s2wake s1c ragg

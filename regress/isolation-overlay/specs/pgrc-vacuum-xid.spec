# SBFIX (pgrc-plans/SBFIX.done): a lazy VACUUM whose transaction is flagged PROC_IN_VACUUM is left out of every other
# backend's snapshot, so it must make no regular catalog update (vacuum.c:1418). pgrcolumnar's VACUUM flushes the
# front: here the flush's rotate reuses slot 0, whose segment the setup's two seals emptied, and resets it (a new
# relfilenode: a pg_class UPDATE under the VACUUM's xid V). v's VACUUM pauses after its flush (injection point
# pgrc-vacuum-flushed, pgrc_front builtins.rs). o commits a transaction begun after V, so V is below its snapshots'
# xmax, and scans pg_class. Were V left out of o's snapshot, the scan would take V for an aborted transaction: it hints
# the segment's new pg_class version invalid and the version V replaced valid (the chain forks, as the primary's did in
# the gate's standby leg). Once the VACUUM commits, slot 0 then reads and writes its pre-reset relfilenode, which the
# commit truncated and left to the next checkpoint to unlink: rseg shows the reset undone, rfile the file gone after
# the CHECKPOINT, and the second CHECKPOINT fails writing r's second row to it. vacuum_rel sets no PROC_IN_VACUUM for a
# pgrcolumnar table, so o's snapshot holds V running and hints nothing: the reset stands and both checkpoints pass.
# Expected output: regress/isolation-overlay/expected/pgrc-vacuum-xid.out. Only pgrc's VACUUM stops at the point and
# a heap table has no segment, so the heap twin's output differs in vvac's wait and the segment rows.

setup { CREATE EXTENSION injection_points; }
setup { CREATE TABLE pgrc_vx (id int8, v text) USING pgrcolumnar; }
setup { INSERT INTO pgrc_vx SELECT g, 'x' FROM generate_series(1, 100) g; }
setup { CALL pgrc_seal('pgrc_vx'); }
setup { CALL pgrc_seal('pgrc_vx'); }
setup { INSERT INTO pgrc_vx SELECT g, 'x' FROM generate_series(101, 200) g; }
setup { CREATE TABLE pgrc_vx_seg AS SELECT relfilenode FROM pg_class WHERE relname = 'pgrc_seg_' || 'pgrc_vx'::regclass::oid || '_0'; }

teardown
{
    DROP TABLE pgrc_vx;
    DROP TABLE pgrc_vx_seg;
    DROP TABLE IF EXISTS pgrc_vx_after;
    DO $$ BEGIN PERFORM injection_points_detach('pgrc-vacuum-flushed'); EXCEPTION WHEN OTHERS THEN NULL; END $$;
    DROP EXTENSION injection_points;
}

session v
step vattach { DO $$ BEGIN PERFORM injection_points_attach('pgrc-vacuum-flushed', 'wait'); END $$; }
step vvac    { VACUUM pgrc_vx; }

session o
setup        { SET enable_indexscan = off; SET enable_indexonlyscan = off; SET enable_bitmapscan = off; }
step odetach { DO $$ BEGIN PERFORM injection_points_detach('pgrc-vacuum-flushed'); END $$; }
step oafter  { CREATE TABLE pgrc_vx_after (i int); }
step oscan   { SELECT count(*) > 0 AS scanned FROM pg_class; }
step owake   { DO $$ BEGIN PERFORM injection_points_wakeup('pgrc-vacuum-flushed'); EXCEPTION WHEN OTHERS THEN NULL; END $$; }

session r
step rseg  { SELECT c.relfilenode <> s.relfilenode AS seg0_reset FROM pg_class c, pgrc_vx_seg s WHERE c.relname = 'pgrc_seg_' || 'pgrc_vx'::regclass::oid || '_0'; }
step rsel  { SELECT count(*), sum(id) FROM pgrc_vx; }
step rins  { INSERT INTO pgrc_vx VALUES (201, 'y'); }
step rchk  { CHECKPOINT; }
step rfile { SELECT (pg_stat_file(pg_relation_filepath(c.oid), true)).size IS NULL AS seg0_file_gone FROM pg_class c WHERE c.relname = 'pgrc_seg_' || 'pgrc_vx'::regclass::oid || '_0'; }
step rins2 { INSERT INTO pgrc_vx VALUES (202, 'z'); }

permutation vattach vvac odetach oafter oscan owake rseg rsel rins rchk rfile rins2 rchk rsel

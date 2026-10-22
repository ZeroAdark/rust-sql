# SBFIX (pgrc-plans/SBFIX.done): vacuum_rel decides PROC_IN_VACUUM from the relation's access method before it takes
# the relation's lock, so an ALTER TABLE ... SET ACCESS METHOD pgrcolumnar committed while the VACUUM waits for that
# lock leaves a flagged VACUUM on a pgrcolumnar table. Its transaction is out of every other snapshot, so it must not
# flush the front (a flush creates or resets segments: regular pg_class updates). RESID-VACFLUSH: the flush runs in
# ordinary transactions of its own before the heap pass's (the flagged one commits first), so this VACUUM flushes too,
# its rotate creating the next segment (2), and its heap pass makes no catalog update. A database-wide VACUUM (vvacall) lists
# the table without locking it, so it reaches vacuum_rel while the ALTER holds its lock; a named one would wait in
# expand_vacuum_rel and see the new access method. The second permutation is the plain case: the VACUUM of a
# pgrcolumnar table flushes, its rotate creating the next segment.
# Expected output: regress/isolation-overlay/expected/pgrc-vacuum-am-race.out. A heap table has no segment, so the heap
# twin's counts differ.

setup { CREATE TABLE pgrc_vr (id int8, v text); }
setup { INSERT INTO pgrc_vr SELECT g, 'x' FROM generate_series(1, 100) g; }

teardown { DROP TABLE pgrc_vr; }

session a
step abegin  { BEGIN; }
step aalter  { ALTER TABLE pgrc_vr SET ACCESS METHOD pgrcolumnar; }
step ains    { INSERT INTO pgrc_vr SELECT g, 'x' FROM generate_series(101, 200) g; }
step acommit { COMMIT; }

session v
step vvacall { VACUUM; }
step vvac    { VACUUM pgrc_vr; }

session r
step rseg { SELECT count(*) AS segments FROM pg_class WHERE relname LIKE 'pgrc\_seg\_' || 'pgrc_vr'::regclass::oid || '\_%'; }
step rsel { SELECT count(*), sum(id) FROM pgrc_vr; }

permutation abegin aalter ains vvacall acommit rseg rsel
permutation abegin aalter ains acommit rseg vvac rseg rsel

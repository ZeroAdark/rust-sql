# LOCK (pgrc delta-store, 2026-10-03): the KEY SHARE residual, pinned (main 2026-10-03: accepted for now; an IX4
# prerequisite, to close before foreign keys referencing a pgrcolumnar table are served). Heap keeps a KEY SHARE lock
# on a row across a concurrent no-key update: the update's new version carries the locker in its xmax, so a later
# DELETE of the new version waits for the locker. On pgrcolumnar the update moves the row to a new RowIdent whose
# LOCK row does not exist: the KEY SHARE holder's lock stays on the old RowIdent, and the DELETE does not wait.
# Likewise a KEY SHARE taken while a no-key update is in progress locks the successor only (heap also keeps the old
# version locked, which shows only if the update aborts). Its expected output,
# regress/isolation-overlay/expected/pgrc-lock-keyshare.out, differs from the heap twin exactly in the marked
# permutations: s3d / s3ds do not wait. The other permutations agree with heap. Rows: id 1 sealed, id 11 segment.

setup { CREATE TABLE pgrc_t (id int, v int) USING pgrcolumnar; }
setup { INSERT INTO pgrc_t VALUES (1, 0); }
setup { CALL pgrc_seal('pgrc_t'); }
setup { INSERT INTO pgrc_t VALUES (11, 0); }

teardown { DROP TABLE pgrc_t; }

session s1
step s1b    { BEGIN; }
step s1ks   { SELECT id, v FROM pgrc_t WHERE id = 1 FOR KEY SHARE; }
step s1kss  { SELECT id, v FROM pgrc_t WHERE id = 11 FOR KEY SHARE; }
step s1c    { COMMIT; }

session s2
step s2b    { BEGIN; }
step s2u    { UPDATE pgrc_t SET v = v + 1 WHERE id = 1; }
step s2us   { UPDATE pgrc_t SET v = v + 1 WHERE id = 11; }
step s2c    { COMMIT; }
step s2a    { ROLLBACK; }

session s3
step s3d    { DELETE FROM pgrc_t WHERE id = 1; }
step s3ds   { DELETE FROM pgrc_t WHERE id = 11; }
step s3dn   { DELETE FROM pgrc_t WHERE v = 1; }

session r
step rsel   { SELECT id, v FROM pgrc_t ORDER BY id; }

# DIFFERS: a KEY SHARE holder, then a committed no-key update; the DELETE of the new version waits on heap only
permutation s1b s1ks s2u s3d s1c rsel
permutation s1b s1kss s2us s3ds s1c rsel
# agrees: the update in progress when KEY SHARE locks; its successor is locked, so a DELETE after it commits waits
permutation s2b s2u s1b s1ks s2c s3dn s1c rsel
# agrees: the update in progress aborts; the old version is not locked here, but no one writes it before s1 ends
permutation s2b s2u s1b s1ks s2a s1c rsel

# LOCK (pgrc delta-store, 2026-10-03): PG's lock-update-delete and lock-update-traversal on a pgrcolumnar twin (one
# row, key 1, in a sealed part; the twins have no primary key, so every update is a no-key update on both). A KEY
# SHARE lock taken while an update chain is in progress, under the pg_advisory dance; and a REPEATABLE READ locker
# whose snapshot predates a committed update (40001), then a delete / update of the row it locked. Expected output: the
# heap twin, except where pgrc-lock-keyshare pins the KEY SHARE residual (scripts/pgrc-isolation.sh).

setup { CREATE TABLE foo (key int, value int) USING pgrcolumnar; }
setup { INSERT INTO foo VALUES (1, 1); }
setup { CALL pgrc_seal('foo'); }

teardown { DROP TABLE foo; }

session s1
step s1l    { SELECT * FROM foo WHERE pg_advisory_xact_lock(0) IS NOT NULL AND key = 1 FOR KEY SHARE; }
step s1b    { BEGIN ISOLATION LEVEL REPEATABLE READ; }
step s1s    { SELECT * FROM foo; }
step s1ks   { SELECT * FROM foo FOR KEY SHARE; }
step s1c    { COMMIT; }

session s2
setup       { SELECT pg_advisory_lock(0); }
step s2b    { BEGIN; }
step s2u    { UPDATE foo SET value = 2 WHERE key = 1; }
step s2_blocker1 { DELETE FROM foo; }
step s2_blocker3 { UPDATE foo SET value = 2 WHERE key = 1; }
step s2_unlock { SELECT pg_advisory_unlock(0); }
step s2c    { COMMIT; }
step s2r    { ROLLBACK; }
step s2d1   { DELETE FROM foo WHERE key = 1; }
step s2d3   { UPDATE foo SET value = 3 WHERE key = 1; }

session r
step rsel   { SELECT * FROM foo ORDER BY key; }

# lock-update-delete
permutation s2b s1l s2u s2_blocker1 s2_unlock s2c rsel
permutation s2b s1l s2u s2_blocker3 s2_unlock s2c rsel
permutation s2b s1l s2u s2_blocker1 s2_unlock s2r rsel
permutation s2b s1l s2u s2_blocker3 s2_unlock s2r rsel
permutation s2b s1l s2u s2_blocker1 s2c s2_unlock rsel
permutation s2b s1l s2u s2_blocker3 s2c s2_unlock rsel
permutation s2b s1l s2u s2_blocker1 s2r s2_unlock rsel
permutation s2b s1l s2u s2_blocker3 s2r s2_unlock rsel
# lock-update-traversal
permutation s2_unlock s1b s2b s1s s2u s1ks s2c s2d1 s1c rsel
permutation s2_unlock s1b s2b s1s s2u s1ks s2c s2d3 s1c rsel

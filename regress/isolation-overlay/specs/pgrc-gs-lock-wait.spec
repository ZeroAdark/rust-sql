# TESTMX (PGRCSERVE M3 L4): row-lock waits on pgrcolumnar rows once Gravity serves LockRows on pgrc. Rows 4 (sealed
# part) and 14 (front segment). FOR SHARE waits behind FOR UPDATE and then reads the committed update; FOR KEY SHARE
# does not wait behind FOR NO KEY UPDATE but waits behind FOR UPDATE; a no-key UPDATE does not wait behind FOR KEY
# SHARE, a DELETE does. (The KEY SHARE residual of pgrc-lock-keyshare, a DELETE after a no-key update, is not
# exercised here.) Seeded by COPY. Expected output: the heap twin (scripts/pgrc-isolation.sh).

setup { CREATE OR REPLACE FUNCTION gs_noop() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RETURN NULL; END $$; }
setup { CREATE TABLE gs_t (id int, v int NOT NULL DEFAULT 0) USING pgrcolumnar; }
setup { COPY gs_t (id) FROM PROGRAM 'seq 1 10'; }
setup { CALL pgrc_seal('gs_t'); }
setup { CREATE TRIGGER gs_front AFTER INSERT ON gs_t FOR EACH ROW EXECUTE FUNCTION gs_noop(); }
setup { COPY gs_t (id) FROM PROGRAM 'seq 11 20'; }
setup { DROP TRIGGER gs_front ON gs_t; }

teardown { DROP TABLE gs_t; }

session s1
step s1b   { BEGIN; }
step s1fu  { SELECT id, v FROM gs_t WHERE id IN (4, 14) ORDER BY id FOR UPDATE; }
step s1nk  { SELECT id, v FROM gs_t WHERE id IN (4, 14) ORDER BY id FOR NO KEY UPDATE; }
step s1ks  { SELECT id, v FROM gs_t WHERE id IN (4, 14) ORDER BY id FOR KEY SHARE; }
step s1u   { UPDATE gs_t SET v = v + 1 WHERE id IN (4, 14); }
step s1c   { COMMIT; }

session s2
step s2b   { BEGIN; }
step s2sh  { SELECT id, v FROM gs_t WHERE id IN (4, 14) ORDER BY id FOR SHARE; }
step s2ks  { SELECT id, v FROM gs_t WHERE id IN (4, 14) ORDER BY id FOR KEY SHARE; }
step s2u   { UPDATE gs_t SET v = v + 10 WHERE id = 14 RETURNING id, v; }
step s2d   { DELETE FROM gs_t WHERE id = 4 RETURNING id, v; }
step s2c   { COMMIT; }

session r
step rsel  { SELECT id, v FROM gs_t WHERE id IN (4, 14) ORDER BY id; }

# FOR SHARE waits behind FOR UPDATE, then reads the update s1 committed
permutation s1b s1fu s2b s2sh s1u s1c s2c rsel
# FOR KEY SHARE: not behind FOR NO KEY UPDATE, behind FOR UPDATE
permutation s1b s1nk s2b s2ks s2c s1c rsel
permutation s1b s1fu s2b s2ks s1c s2c rsel
# behind FOR KEY SHARE: a no-key UPDATE goes on, a DELETE waits
permutation s1b s1ks s2b s2u s2c s1c rsel
permutation s1b s1ks s2b s2d s1c s2c rsel

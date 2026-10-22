# TESTMX (PGRCSERVE M3 L4): NOWAIT on pgrcolumnar rows once Gravity serves LockRows on pgrc. Rows 5 (sealed part) and
# 15 (front segment). FOR UPDATE NOWAIT fails (55P03) behind a held FOR UPDATE and behind an in-progress UPDATE;
# FOR SHARE NOWAIT shares a held FOR SHARE; after the holder commits NOWAIT locks. Seeded by COPY. Expected output:
# the heap twin (scripts/pgrc-isolation.sh).

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
step s1fu  { SELECT id, v FROM gs_t WHERE id IN (5, 15) ORDER BY id FOR UPDATE; }
step s1sh  { SELECT id, v FROM gs_t WHERE id IN (5, 15) ORDER BY id FOR SHARE; }
step s1u   { UPDATE gs_t SET v = v + 1 WHERE id = 15; }
step s1c   { COMMIT; }

session s2
step s2b   { BEGIN; }
step s2np  { SELECT id, v FROM gs_t WHERE id = 5 FOR UPDATE NOWAIT; }
step s2ns  { SELECT id, v FROM gs_t WHERE id = 15 FOR UPDATE NOWAIT; }
step s2shn { SELECT id, v FROM gs_t WHERE id IN (5, 15) ORDER BY id FOR SHARE NOWAIT; }
step s2c   { COMMIT; }

permutation s1b s1fu s2b s2np s1c s2c
permutation s1b s1fu s2b s2ns s1c s2c
permutation s1b s1sh s2b s2shn s2ns s1c s2c
permutation s1b s1u s2b s2ns s1c s2c
permutation s1b s1fu s1c s2b s2np s2ns s2c

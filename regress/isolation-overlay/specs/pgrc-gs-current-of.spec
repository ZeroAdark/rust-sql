# TESTMX (PGRCSERVE M3 L4): WHERE CURRENT OF on pgrcolumnar rows under concurrency, once Gravity serves cursors over
# pgrc and UPDATE / DELETE ... WHERE CURRENT OF (incl. NO SCROLL cursors without FOR UPDATE). Rows 6 (sealed part) and
# 16 (front segment).
#   - a FOR UPDATE cursor's fetched row is locked: another session's UPDATE waits, then follows the cursor's UPDATE
#   - a DELETE WHERE CURRENT OF commits under a waiting UPDATE: no row
#   - a NO SCROLL cursor (no lock) over a row another session updates and commits: the CURRENT OF UPDATE rechecks
#     on the new version
# Seeded by COPY. Expected output: the heap twin (scripts/pgrc-isolation.sh).

setup { CREATE OR REPLACE FUNCTION gs_noop() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RETURN NULL; END $$; }
setup { CREATE TABLE gs_t (id int, v int NOT NULL DEFAULT 0) USING pgrcolumnar; }
setup { COPY gs_t (id) FROM PROGRAM 'seq 1 10'; }
setup { CALL pgrc_seal('gs_t'); }
setup { CREATE TRIGGER gs_front AFTER INSERT ON gs_t FOR EACH ROW EXECUTE FUNCTION gs_noop(); }
setup { COPY gs_t (id) FROM PROGRAM 'seq 11 20'; }
setup { DROP TRIGGER gs_front ON gs_t; }

teardown { DROP TABLE gs_t; }

session s1
step s1b    { BEGIN; }
step s1decl { DECLARE c CURSOR FOR SELECT id, v FROM gs_t WHERE id IN (6, 16) ORDER BY id FOR UPDATE; }
step s1f    { FETCH 1 FROM c; }
step s1cu   { UPDATE gs_t SET v = v + 1 WHERE CURRENT OF c RETURNING id, v; }
step s1cd   { DELETE FROM gs_t WHERE CURRENT OF c RETURNING id, v; }
step s1dns  { DECLARE d NO SCROLL CURSOR FOR SELECT id, v FROM gs_t WHERE id = 16; }
step s1fd   { FETCH 1 FROM d; }
step s1ud   { UPDATE gs_t SET v = v + 1 WHERE CURRENT OF d RETURNING id, v; }
step s1c    { COMMIT; }

session s2
step s2b    { BEGIN; }
step s2u6   { UPDATE gs_t SET v = v + 10 WHERE id = 6 RETURNING id, v; }
step s2u16  { UPDATE gs_t SET v = v + 10 WHERE id = 16 RETURNING id, v; }
step s2c    { COMMIT; }

session r
step rsel   { SELECT id, v FROM gs_t WHERE id IN (6, 16) ORDER BY id; }

permutation s1b s1decl s1f s2b s2u6 s1cu s1c s2c rsel
permutation s1b s1decl s1f s1f s1cd s2b s2u16 s1c s2c rsel
permutation s1b s1decl s1f s1f s1cu s1c rsel
permutation s1b s1dns s1fd s1ud s1c rsel
permutation s2b s2u16 s1b s1dns s1fd s2c s1ud s1c rsel

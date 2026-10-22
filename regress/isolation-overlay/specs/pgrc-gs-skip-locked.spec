# TESTMX (PGRCSERVE M3 L4): SKIP LOCKED on pgrcolumnar rows once Gravity serves LockRows on pgrc: the job-queue
# pattern. Rows 1-5 in a sealed part, 6-10 in the front segment. Two takers each get the first row the other has
# not locked; a taker skips rows another holds and rows an in-progress UPDATE holds; DELETE ... WHERE id IN (SELECT
# ... LIMIT 1 FOR UPDATE SKIP LOCKED) consumes a different row per session. Seeded by COPY. Expected output: the heap
# twin (scripts/pgrc-isolation.sh).

setup { CREATE OR REPLACE FUNCTION gs_noop() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RETURN NULL; END $$; }
setup { CREATE TABLE gs_q (id int, v int NOT NULL DEFAULT 0) USING pgrcolumnar; }
setup { COPY gs_q (id) FROM PROGRAM 'seq 1 5'; }
setup { CALL pgrc_seal('gs_q'); }
setup { CREATE TRIGGER gs_front AFTER INSERT ON gs_q FOR EACH ROW EXECUTE FUNCTION gs_noop(); }
setup { COPY gs_q (id) FROM PROGRAM 'seq 6 10'; }
setup { DROP TRIGGER gs_front ON gs_q; }

teardown { DROP TABLE gs_q; }

session s1
step s1b    { BEGIN; }
step s1t    { SELECT id FROM gs_q ORDER BY id LIMIT 1 FOR UPDATE SKIP LOCKED; }
step s1t2   { SELECT id FROM gs_q ORDER BY id LIMIT 2 FOR UPDATE SKIP LOCKED; }
step s1del  { DELETE FROM gs_q WHERE id IN (SELECT id FROM gs_q ORDER BY id LIMIT 1 FOR UPDATE SKIP LOCKED) RETURNING id; }
step s1c    { COMMIT; }

session s2
step s2b    { BEGIN; }
step s2t    { SELECT id FROM gs_q ORDER BY id LIMIT 1 FOR UPDATE SKIP LOCKED; }
step s2all  { SELECT id FROM gs_q WHERE id <= 8 ORDER BY id FOR UPDATE SKIP LOCKED; }
step s2del  { DELETE FROM gs_q WHERE id IN (SELECT id FROM gs_q ORDER BY id LIMIT 1 FOR UPDATE SKIP LOCKED) RETURNING id; }
step s2c    { COMMIT; }

session s3
step s3b    { BEGIN; }
step s3u    { UPDATE gs_q SET v = 1 WHERE id IN (2, 7); }
step s3c    { COMMIT; }

session r
step rsel   { SELECT id, v FROM gs_q ORDER BY id; }

permutation s1b s2b s1t s2t s1c s2c
permutation s1b s2b s1t2 s2all s1c s2c
permutation s1b s2b s1del s2del s1c s2c rsel
permutation s3b s3u s2b s2all s3c s2c
permutation s3b s3u s1b s1t2 s3c s1c

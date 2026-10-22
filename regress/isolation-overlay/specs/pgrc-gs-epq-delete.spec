# TESTMX (PGRCSERVE M3 L4): UPDATE || DELETE, both orders, on a pgrcolumnar row once Gravity serves pgrc DML. Row 3 is
# in a sealed part, row 13 in the front segment; every statement is fed by an index scan (enable_seqscan off). A
# waiting UPDATE finds the row deleted (no row); a waiting DELETE whose qual no longer holds on the new version
# deletes nothing; a waiting DELETE without that qual deletes the new version; an aborted first writer lets the
# waiter act on the original. Seeded by COPY. Expected output: the heap twin (scripts/pgrc-isolation.sh).

setup { CREATE OR REPLACE FUNCTION gs_noop() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RETURN NULL; END $$; }
setup { CREATE TABLE gs_t (id int, v int NOT NULL DEFAULT 0) USING pgrcolumnar; }
setup { COPY gs_t (id) FROM PROGRAM 'seq 1 10'; }
setup { CALL pgrc_seal('gs_t'); }
setup { CREATE TRIGGER gs_front AFTER INSERT ON gs_t FOR EACH ROW EXECUTE FUNCTION gs_noop(); }
setup { COPY gs_t (id) FROM PROGRAM 'seq 11 20'; }
setup { DROP TRIGGER gs_front ON gs_t; }
setup { CREATE INDEX gs_t_id ON gs_t (id); ANALYZE gs_t; }

teardown { DROP TABLE gs_t; }

session s1
step s1b   { BEGIN; SET LOCAL enable_seqscan = off; }
step s1dp  { DELETE FROM gs_t WHERE id = 3 RETURNING id, v; }
step s1ds  { DELETE FROM gs_t WHERE id = 13 RETURNING id, v; }
step s1up  { UPDATE gs_t SET v = v + 1 WHERE id = 3 RETURNING id, v; }
step s1us  { UPDATE gs_t SET v = v + 1 WHERE id = 13 RETURNING id, v; }
step s1c   { COMMIT; }
step s1a   { ROLLBACK; }

session s2
step s2b   { BEGIN; SET LOCAL enable_seqscan = off; }
step s2up  { UPDATE gs_t SET v = v + 10 WHERE id = 3 RETURNING id, v; }
step s2us  { UPDATE gs_t SET v = v + 10 WHERE id = 13 RETURNING id, v; }
step s2qp  { DELETE FROM gs_t WHERE id = 3 AND v = 0 RETURNING id, v; }
step s2qs  { DELETE FROM gs_t WHERE id = 13 AND v = 0 RETURNING id, v; }
step s2ds  { DELETE FROM gs_t WHERE id = 13 RETURNING id, v; }
step s2c   { COMMIT; }

session r
step rsel  { SELECT id, v FROM gs_t WHERE id IN (3, 13) ORDER BY id; }

# a committed DELETE: the waiting UPDATE finds no row
permutation s1b s1dp s2b s2up s1c s2c rsel
permutation s1b s1ds s2b s2us s1c s2c rsel
# a committed UPDATE: the waiting DELETE rechecks its qual on the new version
permutation s1b s1up s2b s2qp s1c s2c rsel
permutation s1b s1us s2b s2qs s1c s2c rsel
permutation s1b s1us s2b s2ds s1c s2c rsel
# the first writer aborts
permutation s1b s1dp s2b s2up s1a s2c rsel
permutation s1b s1us s2b s2qs s1a s2c rsel

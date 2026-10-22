# TESTMX (PGRCSERVE M3 L4): UPDATE || UPDATE on a pgrcolumnar row once Gravity serves pgrc DML. Row 2 is in a sealed
# part, row 12 in the front segment; the writers' UPDATEs are fed by an index scan (enable_seqscan off) or joined to
# a heap table. READ COMMITTED: the waiter re-evaluates the committed version (EPQ): its new value, or no row when
# the recheck fails; the first writer's abort lets the waiter act on the original. REPEATABLE READ: 40001. The table
# is seeded by COPY (a COPY through an AFTER ROW trigger keeps its rows in the segment), so no INSERT is needed.
# Expected output: the heap twin (scripts/pgrc-isolation.sh).

setup { CREATE OR REPLACE FUNCTION gs_noop() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RETURN NULL; END $$; }
setup { CREATE TABLE gs_t (id int, v int NOT NULL DEFAULT 0) USING pgrcolumnar; }
setup { COPY gs_t (id) FROM PROGRAM 'seq 1 10'; }
setup { CALL pgrc_seal('gs_t'); }
setup { CREATE TRIGGER gs_front AFTER INSERT ON gs_t FOR EACH ROW EXECUTE FUNCTION gs_noop(); }
setup { COPY gs_t (id) FROM PROGRAM 'seq 11 20'; }
setup { DROP TRIGGER gs_front ON gs_t; }
setup { CREATE INDEX gs_t_id ON gs_t (id); ANALYZE gs_t; }
setup { CREATE TABLE gs_src (id int, w int); INSERT INTO gs_src SELECT g, g * 100 FROM generate_series(1, 20) g; }

teardown { DROP TABLE gs_t; DROP TABLE gs_src; }

session s1
step s1b   { BEGIN; SET LOCAL enable_seqscan = off; }
step s1p   { UPDATE gs_t SET v = v + 1 WHERE id = 2 RETURNING id, v; }
step s1s   { UPDATE gs_t SET v = v + 1 WHERE id = 12 RETURNING id, v; }
step s1c   { COMMIT; }
step s1a   { ROLLBACK; }

session s2
step s2b   { BEGIN; SET LOCAL enable_seqscan = off; }
step s2rr  { BEGIN ISOLATION LEVEL REPEATABLE READ; SET LOCAL enable_seqscan = off; SELECT count(*) FROM gs_t; }
step s2p   { UPDATE gs_t SET v = v + 10 WHERE id = 2 RETURNING id, v; }
step s2s   { UPDATE gs_t SET v = v + 10 WHERE id = 12 RETURNING id, v; }
step s2jp  { UPDATE gs_t t SET v = t.v + s.w FROM gs_src s WHERE s.id = t.id AND t.id = 2 RETURNING t.id, t.v; }
step s2js  { UPDATE gs_t t SET v = t.v + s.w FROM gs_src s WHERE s.id = t.id AND t.id = 12 RETURNING t.id, t.v; }
step s2qp  { UPDATE gs_t SET v = v + 100 WHERE id = 2 AND v = 0 RETURNING id, v; }
step s2qs  { UPDATE gs_t SET v = v + 100 WHERE id = 12 AND v = 0 RETURNING id, v; }
step s2c   { COMMIT; }

session r
step rsel  { SELECT id, v FROM gs_t WHERE id IN (2, 12) ORDER BY id; }

# READ COMMITTED, index-fed: the waiter follows the committed version
permutation s1b s1p s2b s2p s1c s2c rsel
permutation s1b s1s s2b s2s s1c s2c rsel
# a join-fed waiter follows it too
permutation s1b s1p s2b s2jp s1c s2c rsel
permutation s1b s1s s2b s2js s1c s2c rsel
# the waiter's recheck fails on the new version: no row
permutation s1b s1p s2b s2qp s1c s2c rsel
permutation s1b s1s s2b s2qs s1c s2c rsel
# the first writer aborts: the waiter acts on the original row
permutation s1b s1s s2b s2qs s1a s2c rsel
# REPEATABLE READ: the waiter's snapshot predates the commit
permutation s2rr s1b s1p s2p s1c s2c rsel
permutation s2rr s1b s1s s2s s1c s2c rsel

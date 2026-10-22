# TRIGGERS: a foreign key from a pgrcolumnar table to a heap table, against the heap twin. Deleting a referenced row
# runs the RI NO ACTION check, SELECT 1 FROM <fk> x WHERE ... FOR KEY SHARE OF x. Its lock waits out a referencing
# row's in-progress delete as heap's does (commit: the row is gone and the delete succeeds; abort: 23503), and does not
# wait for an in-progress update (no unique index, so every update is a no-key update): the version reached counts.
# Referencing rows: pid 1 in a sealed part (id 1), pid 2 in the live segment (id 11).

setup { CREATE TABLE pgrc_pk (id int PRIMARY KEY); INSERT INTO pgrc_pk SELECT generate_series(1, 5); }
setup { CREATE TABLE pgrc_fk (id int, pid int REFERENCES pgrc_pk (id)) USING pgrcolumnar; }
setup { INSERT INTO pgrc_fk VALUES (1, 1); }
setup { CALL pgrc_seal('pgrc_fk'); }
setup { INSERT INTO pgrc_fk VALUES (11, 2); }

teardown { DROP TABLE pgrc_fk; DROP TABLE pgrc_pk; }

session s1
step s1b    { BEGIN; }
step s1dp   { DELETE FROM pgrc_fk WHERE id = 1; }
step s1ds   { DELETE FROM pgrc_fk WHERE id = 11; }
step s1up   { UPDATE pgrc_fk SET pid = 3 WHERE id = 1; }
step s1c    { COMMIT; }
step s1a    { ROLLBACK; }

session s2
step s2dp   { DELETE FROM pgrc_pk WHERE id = 1; }
step s2ds   { DELETE FROM pgrc_pk WHERE id = 2; }

session r
step rsel   { SELECT (SELECT string_agg(id::text, ',' ORDER BY id) FROM pgrc_pk) AS pk, (SELECT string_agg(id || ':' || pid, ',' ORDER BY id) FROM pgrc_fk) AS fk; }

permutation s1b s1dp s2dp s1c rsel
permutation s1b s1dp s2dp s1a rsel
permutation s1b s1ds s2ds s1c rsel
permutation s1b s1ds s2ds s1a rsel
permutation s1b s1up s2dp s1c rsel

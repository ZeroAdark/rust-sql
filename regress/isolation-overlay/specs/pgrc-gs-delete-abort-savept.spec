# TESTMX (PGRCSERVE M3 L4): PostgreSQL's delete-abort-savept on a pgrcolumnar table once Gravity serves pgrc DML and
# LockRows: a FOR KEY SHARE lock outlives a DELETE rolled back to a savepoint, so a FOR UPDATE waits for the
# transaction, not the savepoint. Key 1 is in a sealed part, key 2 in the front segment (no PRIMARY KEY: pgrc has no
# unique index). Seeded by COPY. Expected output: the heap twin (scripts/pgrc-isolation.sh).

setup { CREATE OR REPLACE FUNCTION gs_noop() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RETURN NULL; END $$; }
setup { CREATE TABLE gs_foo (key int NOT NULL, value int) USING pgrcolumnar; }
setup { COPY gs_foo FROM PROGRAM 'echo 1,1' WITH (FORMAT csv); }
setup { CALL pgrc_seal('gs_foo'); }
setup { CREATE TRIGGER gs_front AFTER INSERT ON gs_foo FOR EACH ROW EXECUTE FUNCTION gs_noop(); }
setup { COPY gs_foo FROM PROGRAM 'echo 2,2' WITH (FORMAT csv); }
setup { DROP TRIGGER gs_front ON gs_foo; }

teardown { DROP TABLE gs_foo; }

session s1
setup       { BEGIN; }
step s1l    { SELECT * FROM gs_foo ORDER BY key FOR KEY SHARE; }
step s1svp  { SAVEPOINT f; }
step s1d    { DELETE FROM gs_foo; }
step s1r    { ROLLBACK TO f; }
step s1c    { COMMIT; }

session s2
setup       { BEGIN; }
step s2l    { SELECT * FROM gs_foo ORDER BY key FOR UPDATE; }
step s2c    { COMMIT; }

permutation s1l s1svp s1d s1r s1c s2l s2c
permutation s1l s1svp s1d s1r s2l s1c s2c
permutation s1l s1svp s1d s2l s1r s1c s2c
permutation s1l s1svp s2l s1d s1r s1c s2c
permutation s1l s2l s1svp s1d s1r s1c s2c
permutation s2l s1l s2c s1svp s1d s1r s1c
permutation s2l s2c s1l s1svp s1d s1r s1c

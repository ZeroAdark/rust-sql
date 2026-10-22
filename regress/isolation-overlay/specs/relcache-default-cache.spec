# pglguc C5b: a backend reads each column default (pg_attrdef.adbin) once into its relcache and copies it per use. Every
# change to a default must reach the next INSERT, in the session that made it and in another session whose cache already
# holds the old default: SET / DROP DEFAULT, ALTER COLUMN TYPE (the default is re-cooked), a sequence rename, a sequence
# dropped (CASCADE drops the default) and recreated under the same name, a default calling a function that is then
# CREATE OR REPLACEd, a SET DEFAULT rolled back, and a relcache flush of the table (ANALYZE) followed by a reuse.
setup
{
 CREATE SEQUENCE dseq;
 CREATE FUNCTION dfn() RETURNS int LANGUAGE sql AS 'SELECT 1';
 CREATE TABLE dt (id serial, a int, b int DEFAULT 7, c int DEFAULT dfn(), d bigint DEFAULT nextval('dseq'));
}

teardown
{
 DROP TABLE dt;
 DROP SEQUENCE IF EXISTS dseq, dseq2;
 DROP FUNCTION dfn();
}

session s1
step s1i	{ INSERT INTO dt (a) VALUES (1) RETURNING *; }
step s1b	{ BEGIN; }
step s1rb	{ ROLLBACK; }
step s1set	{ ALTER TABLE dt ALTER COLUMN b SET DEFAULT 8; }
step s1type	{ ALTER TABLE dt ALTER COLUMN b TYPE text; }
step s1drop	{ ALTER TABLE dt ALTER COLUMN b DROP DEFAULT; }
step s1ren	{ ALTER SEQUENCE dseq RENAME TO dseq2; }
step s1fn	{ CREATE OR REPLACE FUNCTION dfn() RETURNS int LANGUAGE sql AS 'SELECT 2'; }
step s1seqdrop	{ DROP SEQUENCE dseq2 CASCADE; CREATE SEQUENCE dseq2 START 100; }
step s1seqset	{ ALTER TABLE dt ALTER COLUMN d SET DEFAULT nextval('dseq2'); }
step s1an	{ ANALYZE dt; }

session s2
step s2i	{ INSERT INTO dt (a) VALUES (2) RETURNING *; }
step s2set	{ ALTER TABLE dt ALTER COLUMN b SET DEFAULT 8; }
step s2type	{ ALTER TABLE dt ALTER COLUMN b TYPE text; }
step s2drop	{ ALTER TABLE dt ALTER COLUMN b DROP DEFAULT; }
step s2ren	{ ALTER SEQUENCE dseq RENAME TO dseq2; }
step s2fn	{ CREATE OR REPLACE FUNCTION dfn() RETURNS int LANGUAGE sql AS 'SELECT 2'; }
step s2seqdrop	{ DROP SEQUENCE dseq2 CASCADE; CREATE SEQUENCE dseq2 START 100; }
step s2seqset	{ ALTER TABLE dt ALTER COLUMN d SET DEFAULT nextval('dseq2'); }
step s2an	{ ANALYZE dt; }

permutation s1i s1set s1i s1type s1i s1drop s1i s1ren s1i s1fn s1i s1seqdrop s1i s1seqset s1i
permutation s1i s2i s2set s1i s2type s1i s2drop s1i s2ren s1i s2fn s1i s2seqdrop s1i s2seqset s1i s2i
permutation s1i s1b s1set s1i s1rb s1i s2i
permutation s1i s2i s1an s1i s2an s1i s2i

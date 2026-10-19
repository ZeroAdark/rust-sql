-- bugs/batch-189-backend-catalog: catalog fixes, expected captured from C 18.6.
\set VERBOSITY verbose
-- regression: pg_constraint.c get_domain_constraint_oid
-- passes the name to nameeq unchanged; a 64-byte name never matches (C
-- reports 42704, pgrust panicked in name_arg).
CREATE DOMAIN b189_d AS int;
DO $$BEGIN EXECUTE format('ALTER DOMAIN b189_d ADD CONSTRAINT %I CHECK (VALUE > 0)', repeat('c',63)); END$$;
SELECT * FROM pg_get_object_address('domain constraint', ARRAY['b189_d'], ARRAY[repeat('c',64)]);
SELECT * FROM pg_get_object_address('domain constraint', ARRAY['b189_d'], ARRAY[repeat('c',70)]);
SELECT classid, objsubid, objid = (SELECT oid FROM pg_constraint WHERE conname = repeat('c',63)) AS same FROM pg_get_object_address('domain constraint', ARRAY['b189_d'], ARRAY[repeat('c',63)]);
DROP DOMAIN b189_d;
-- regression: pg_publication.c:1171 pg_get_publication_tables
-- looks the name bytes up as-is; a non-UTF-8 name in a SQL_ASCII database is
-- C's 42704 (pgrust panicked on from_utf8).
CREATE DATABASE b189_ascii TEMPLATE template0 ENCODING 'SQL_ASCII' LC_COLLATE 'C' LC_CTYPE 'C';
\c b189_ascii
SELECT pg_get_publication_tables(convert_from(decode('70ff','hex'),'SQL_ASCII'));
SELECT pg_get_publication_tables('p' || convert_from(decode('fe','hex'),'SQL_ASCII'));
SELECT pg_get_publication_tables('b189_nopub');
\c postgres
DROP DATABASE b189_ascii;

-- bugs/batch-64-backend-catalog: catalog fixes, expected captured from C 18.6.
\set VERBOSITY verbose
-- regression: aclchk.c:937 errorConflictingDefElem(defel, pstate)
-- reports a character position, not the scanner's byte offset.
/*é*/ ALTER DEFAULT PRIVILEGES IN SCHEMA public IN SCHEMA public GRANT SELECT ON TABLES TO PUBLIC;
/*é*/ ALTER DEFAULT PRIVILEGES FOR ROLE postgres FOR ROLE postgres GRANT SELECT ON TABLES TO PUBLIC;
-- regression: index.c:410 CheckAttributeType(flags=0) recurses
-- into a composite expression's fields.
CREATE TABLE b64_t(a int);
CREATE INDEX b64_t_bad ON b64_t ((NULL::pg_catalog.pg_statistic) pg_catalog.record_ops);
SELECT count(*) AS indexes FROM pg_index WHERE indrelid = 'b64_t'::regclass;
DROP TABLE b64_t;
-- regression: objectaddress.c:1617 LookupTypeName(missing_ok)
-- lets a nonexistent explicit schema raise 3F000.
SELECT * FROM pg_get_object_address('type', ARRAY['b64_absent_schema.t'], ARRAY[]::text[]);
SELECT * FROM pg_get_object_address('type', ARRAY['public.b64_absent_type'], ARRAY[]::text[]);
-- regression: objectaddress.c:1702 atoi is (int) strtol.
SELECT * FROM pg_get_object_address('operator of access method', ARRAY['btree','pg_catalog','integer_ops','-2147483648'], ARRAY['integer','integer']);
SELECT * FROM pg_get_object_address('operator of access method', ARRAY['btree','pg_catalog','integer_ops','2147483648'], ARRAY['integer','integer']);
SELECT * FROM pg_get_object_address('function of access method', ARRAY['btree','pg_catalog','integer_ops','99999999999999999999'], ARRAY['integer','integer']);
-- regression: pg_enum.c:172 validates every label of a batch
-- before the batch is inserted.
CREATE TYPE b64_e AS ENUM ('a', 'a', 'abcdefghijklmnopqrstuvwxyzabcdefghijklmnopqrstuvwxyzabcdefghijkl');
-- regression: pg_publication.c:945 GetPublicationSchemas
-- yields schemas in (pnnspid, pnpubid) index order.
CREATE SCHEMA b64_s1; CREATE SCHEMA b64_s2;
CREATE TABLE b64_s1.t(a int); CREATE TABLE b64_s2.t(a int);
CREATE PUBLICATION b64_p FOR TABLES IN SCHEMA b64_s2;
ALTER PUBLICATION b64_p ADD TABLES IN SCHEMA b64_s1;
SELECT relid::regclass, ordinality FROM pg_get_publication_tables('b64_p') WITH ORDINALITY ORDER BY ordinality;
DROP PUBLICATION b64_p;
DROP SCHEMA b64_s1 CASCADE; DROP SCHEMA b64_s2 CASCADE;
-- regression: cluster.c:774 make_new_heap passes use_user_acl=false,
-- so a rewrite heap carries no default ACL.
CREATE TABLE b64_rw(a integer); INSERT INTO b64_rw VALUES (1);
ALTER DEFAULT PRIVILEGES GRANT SELECT ON TABLES TO PUBLIC;
CREATE FUNCTION b64_probe(integer) RETURNS bigint LANGUAGE sql VOLATILE AS $$ SELECT count(*) FROM pg_catalog.pg_class WHERE relrewrite = 'b64_rw'::regclass AND relacl IS NOT NULL $$;
ALTER TABLE b64_rw ALTER COLUMN a TYPE bigint USING b64_probe(a);
SELECT a FROM b64_rw;
ALTER DEFAULT PRIVILEGES REVOKE SELECT ON TABLES FROM PUBLIC;
DROP FUNCTION b64_probe(integer); DROP TABLE b64_rw;

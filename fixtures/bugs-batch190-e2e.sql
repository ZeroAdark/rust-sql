-- bugs/batch-190-backend-catalog: catalog fixes, expected captured from C 18.6.
\set VERBOSITY verbose
-- regression: pg_enum.c:726 AtEOXact_Enum — the uncommitted-enum
-- bookkeeping lives in TopTransactionContext, so no context survives the
-- transaction (pgrust kept an "UncommittedEnums" context for the session).
CREATE TYPE b190_e AS ENUM ('a');
SELECT count(*) FROM pg_backend_memory_contexts WHERE name = 'UncommittedEnums';
BEGIN;
ALTER TYPE b190_e ADD VALUE 'b';
ROLLBACK;
SELECT count(*) FROM pg_backend_memory_contexts WHERE name = 'UncommittedEnums';
SELECT enum_range(NULL::b190_e);
DROP TYPE b190_e;
-- regression / regression: pg_proc.c:1124 match_prosrc_to_query
-- counts the statement prefix with pg_mbstrlen_with_len, i.e. bytes in a
-- SQL_ASCII database (pgrust counted UTF-8 characters, shifting the cursor).
CREATE DATABASE b190sa TEMPLATE template0 ENCODING 'SQL_ASCII' LOCALE 'C';
\c b190sa
CREATE FUNCTION "é"() RETURNS int AS 'SELECT 1 +' LANGUAGE sql;
CREATE FUNCTION "éé"() RETURNS int AS $$SELECT 1 +$$ LANGUAGE sql;
\c postgres
DROP DATABASE b190sa;

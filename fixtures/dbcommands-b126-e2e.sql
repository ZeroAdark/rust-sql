-- bugs/batch-126-backend-commands-dbcommands: CREATE DATABASE / ALTER DATABASE
-- option errors carry parser_errposition (dbcommands.c:845/881/902/913/2422),
-- i.e. a character position, not the scanner's byte offset. Expected captured
-- from C 18.6.
CREATE DATABASE "é" ENCODING 'not_an_encoding';
/* é */ CREATE DATABASE b126x ENCODING 999;
/* é */ CREATE DATABASE b126x WITH LOCATION '/x' FOO 1;
/* é */ CREATE DATABASE b126x FOO 1;
/* é */ CREATE DATABASE b126x TEMPLATE t TEMPLATE t;
/* é */ ALTER DATABASE postgres CONNECTION LIMIT 1 CONNECTION LIMIT 2;
/* é */ ALTER DATABASE postgres FOO 1;
/* é */ ALTER DATABASE postgres ALLOW_CONNECTIONS true CONNECTION LIMIT 1 FOO 1;
-- movedb (dbcommands.c:2156): the destination directory walk goes through
-- AllocateDir/ReadDir (errcode_for_file_access on iteration failure).
SET allow_in_place_tablespaces = on;
CREATE TABLESPACE b126_ts LOCATION '';
CREATE DATABASE b126_mv;
\c b126_mv
CREATE TABLE b126_t (a int) TABLESPACE b126_ts;
\c postgres
ALTER DATABASE b126_mv SET TABLESPACE b126_ts;
DROP DATABASE b126_mv;
DROP TABLESPACE b126_ts;

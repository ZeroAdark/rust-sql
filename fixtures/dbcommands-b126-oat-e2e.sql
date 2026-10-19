-- bugs/batch-126-backend-commands-dbcommands: the database object-access
-- hook sites of dbcommands.c (createdb:1510 post-create, dropdb:1725 drop,
-- RenameDatabase:1986 / movedb:2244 / AlterDatabase:2526 post-alter),
-- witnessed through the test_oat_hooks recorder. The C oracle install carries
-- no test modules, so the expected file is captured from pgrust.
LOAD 'test_oat_hooks';
SET allow_in_place_tablespaces = on;
CREATE TABLESPACE b126_oat_ts LOCATION '';
SET test_oat_hooks.audit = on;
CREATE DATABASE b126_oat;
ALTER DATABASE b126_oat CONNECTION LIMIT 2;
ALTER DATABASE b126_oat RENAME TO b126_oat2;
ALTER DATABASE b126_oat2 SET TABLESPACE b126_oat_ts;
DROP DATABASE b126_oat2;
SET test_oat_hooks.audit = off;
DROP TABLESPACE b126_oat_ts;

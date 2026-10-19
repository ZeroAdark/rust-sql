-- postgres_fdw batch-205: a direct-modify RETURNING conversion error names
-- the target's query alias (rangetable eref), like a foreign scan does.
-- Loopback server on this backend.
CREATE EXTENSION postgres_fdw;
DO $d$
BEGIN
  EXECUTE format('CREATE SERVER b205_lb FOREIGN DATA WRAPPER postgres_fdw OPTIONS (dbname %L, port %L, host %L)',
                 current_database(), current_setting('port'), current_setting('unix_socket_directories'));
  EXECUTE format('CREATE USER MAPPING FOR CURRENT_USER SERVER b205_lb OPTIONS (user %L)', current_user);
END
$d$;
CREATE TABLE b205_t (a text);
INSERT INTO b205_t VALUES ('1'), ('bad');
CREATE FOREIGN TABLE b205_ft (a integer) SERVER b205_lb OPTIONS (table_name 'b205_t');
EXPLAIN (VERBOSE, COSTS OFF) DELETE FROM b205_ft AS x RETURNING a;
DELETE FROM b205_ft AS x RETURNING a;
UPDATE b205_ft AS y SET a = a RETURNING a;
DELETE FROM b205_ft RETURNING a;
SELECT * FROM b205_ft AS z;
-- a WARNING raised while converting a direct-modify RETURNING row carries
-- the same alias
\set SHOW_CONTEXT always
CREATE FUNCTION b205_warn(int) RETURNS bool LANGUAGE plpgsql AS $$ BEGIN RAISE WARNING 'b205 %', $1; RETURN true; END $$;
CREATE DOMAIN b205_dom AS int CHECK (b205_warn(VALUE));
CREATE TABLE b205_tw (a int);
INSERT INTO b205_tw VALUES (7);
CREATE FOREIGN TABLE b205_ftw (a b205_dom) SERVER b205_lb OPTIONS (table_name 'b205_tw');
DELETE FROM b205_ftw AS w RETURNING a;
\set SHOW_CONTEXT errors
DROP SERVER b205_lb CASCADE;
DROP TABLE b205_t, b205_tw;
DROP DOMAIN b205_dom;
DROP FUNCTION b205_warn(int);
DROP EXTENSION postgres_fdw;

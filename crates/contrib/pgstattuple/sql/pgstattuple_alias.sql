CREATE EXTENSION pgstattuple;

-- build_pgstattuple_type resolves every output column's input function
-- (TupleDescGetAttInMetadata) before converting any value.
CREATE TYPE pgstattuple_shell;
CREATE TABLE pgstattuple_alias_t(a int);
INSERT INTO pgstattuple_alias_t VALUES (1);
CREATE FUNCTION pgstattuple_alias(text) RETURNS TABLE(table_len date, tuple_count pgstattuple_shell)
  LANGUAGE C STRICT AS '$libdir/pgstattuple', 'pgstattuple';
SELECT * FROM pgstattuple_alias('pgstattuple_alias_t');
CREATE FUNCTION pgstattuple_alias2(text) RETURNS TABLE(table_len date, tuple_count int8)
  LANGUAGE C STRICT AS '$libdir/pgstattuple', 'pgstattuple';
SELECT * FROM pgstattuple_alias2('pgstattuple_alias_t');
CREATE FUNCTION pgstattuple_alias3(text) RETURNS TABLE(table_len int8, tuple_count int8)
  LANGUAGE C STRICT AS '$libdir/pgstattuple', 'pgstattuple';
SELECT * FROM pgstattuple_alias3('pgstattuple_alias_t');
DROP TABLE pgstattuple_alias_t;

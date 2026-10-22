CREATE EXTENSION hstore;

-- setup_firstcall's BlessTupleDesc (hstore_op.c:871) leaves a named
-- composite's typmod alone; only anonymous RECORD descriptors get one.
CREATE TYPE kv AS (key text, value text);
CREATE FUNCTION each_kv(hstore) RETURNS SETOF kv
  AS '$libdir/hstore', 'hstore_each' LANGUAGE C STRICT;
SELECT * FROM each_kv('a=>1, b=>2, c=>NULL');
SELECT each_kv('a=>1, b=>2');
SELECT (each_kv('a=>1')).*;
DROP FUNCTION each_kv(hstore);
DROP TYPE kv;

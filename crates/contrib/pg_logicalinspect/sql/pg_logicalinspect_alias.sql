CREATE EXTENSION pg_logicalinspect;

-- The snapshot file name goes through text_to_cstring: it ends at its
-- first NUL.
CREATE FUNCTION snapshot_meta_alias(bytea) RETURNS record
  LANGUAGE C STRICT AS '$libdir/pg_logicalinspect', 'pg_get_logical_snapshot_meta';
SELECT * FROM snapshot_meta_alias('0-1.snap'::bytea) AS r(magic int4, checksum int8, version int4);
SELECT * FROM snapshot_meta_alias('0-1.snap\000junk'::bytea) AS r(magic int4, checksum int8, version int4);
SELECT * FROM snapshot_meta_alias('0-1.snapjunk'::bytea) AS r(magic int4, checksum int8, version int4);
SELECT * FROM snapshot_meta_alias('\0000-1.snap'::bytea) AS r(magic int4, checksum int8, version int4);

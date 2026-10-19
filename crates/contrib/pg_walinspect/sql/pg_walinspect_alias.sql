CREATE EXTENSION pg_walinspect;

-- pg_get_wal_record_info resolves an anonymous RECORD result from the
-- caller's column definition list (get_call_result_type(fcinfo, ...)).
CREATE FUNCTION wal_record_info_alias(pg_lsn) RETURNS record
  LANGUAGE C STRICT AS '$libdir/pg_walinspect', 'pg_get_wal_record_info';
CREATE TABLE wal_alias_t(a int);
SELECT pg_current_wal_lsn() AS lsn0 \gset
INSERT INTO wal_alias_t VALUES (1);
SELECT start_lsn AS rec_lsn FROM pg_get_wal_records_info(:'lsn0', pg_current_wal_lsn())
  WHERE resource_manager = 'Heap' ORDER BY start_lsn LIMIT 1 \gset
SELECT start_lsn = :'rec_lsn' AS same_lsn, resource_manager, record_type
FROM wal_record_info_alias(:'rec_lsn')
  AS r(start_lsn pg_lsn, end_lsn pg_lsn, prev_lsn pg_lsn, xid xid, resource_manager text,
       record_type text, record_length int4, main_data_length int4, fpi_length int4,
       description text, block_ref text);
SELECT start_lsn = :'rec_lsn' AS same_lsn FROM wal_record_info_alias(:'rec_lsn') AS r(start_lsn pg_lsn, end_lsn pg_lsn);
DROP TABLE wal_alias_t;

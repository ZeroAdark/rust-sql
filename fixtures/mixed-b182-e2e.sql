-- bugs/batch-182-mixed-xhigh: expected captured from C 18.6
-- (scripts/mixed-b182-e2e.sh).
\set VERBOSITY verbose
-- regression: a LANGUAGE internal alias declared RETURNS record
-- resolves its anonymous row type through the column definition list
-- (get_call_result_type(fcinfo, ...) -> ReturnSetInfo.expectedDesc); the
-- slot functions passed no fcinfo and raised XX000 "return type must be a
-- row type" before any of their own checks ran.
CREATE FUNCTION b182_physslot(name, boolean, boolean) RETURNS record
  AS 'pg_create_physical_replication_slot' LANGUAGE internal;
CREATE FUNCTION b182_logslot(name, name, boolean, boolean, boolean) RETURNS record
  AS 'pg_create_logical_replication_slot' LANGUAGE internal;
CREATE FUNCTION b182_advance(name, pg_lsn) RETURNS record
  AS 'pg_replication_slot_advance' LANGUAGE internal;
CREATE FUNCTION b182_copyphys(name, name) RETURNS record
  AS 'pg_copy_physical_replication_slot_b' LANGUAGE internal;
SELECT * FROM b182_physslot('b182_s1', false, false) AS (slot_name name, lsn pg_lsn);
SELECT * FROM b182_advance('b182_s1', '0/1') AS (slot_name name, end_lsn pg_lsn);
SELECT * FROM b182_copyphys('b182_s1', 'b182_s2') AS (slot_name name, lsn pg_lsn);
SELECT * FROM b182_logslot('b182_l1', 'no_such_plugin', true, false, false) AS (slot_name name, lsn pg_lsn);
SELECT slot_name, slot_type, temporary FROM pg_replication_slots WHERE slot_name LIKE 'b182%' ORDER BY 1;
SELECT pg_drop_replication_slot('b182_s1');

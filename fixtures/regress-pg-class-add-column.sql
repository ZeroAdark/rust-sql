-- regression: ALTER TABLE pg_class ADD COLUMN under allow_system_table_mods fails
-- like C (cache lookup failed for type 0), not with a Rust panic message.
\set VERBOSITY verbose
\pset format unaligned
\pset footer off
SET statement_timeout = '120s';
SET allow_system_table_mods = on;
ALTER TABLE pg_catalog.pg_class ADD COLUMN hc_ctl_col int;   -- PostgreSQL: ERROR XX000 cache lookup failed for type 0; pgrust: ERROR XX000 index out of bounds: the len is 34 but the index is 34 (server log: panicked)
SELECT 1 AS still_alive;   -- both: 1

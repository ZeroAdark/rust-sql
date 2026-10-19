-- Two-binary differential corpus for the 2026-09-13 bug-inventory batch
-- batch-138-backend-access-transam (C 18.6 oracle vs pgrust).
\set VERBOSITY verbose
-- regression: the auto-tuned wal_buffers count is registered as
-- the dynamic default (xlog.c:4922-4931), not left at -1 in the registry
SHOW wal_buffers;
SELECT setting, unit, source, reset_val, boot_val FROM pg_settings WHERE name = 'wal_buffers';

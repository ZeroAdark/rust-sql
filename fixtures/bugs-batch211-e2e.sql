-- Differential fixture for the 2026-09-13 audit bug batch 211
-- (timezone/pgtz): every backend carries the "Timezones" context that C's
-- init_timezone_hashtable creates (pgtz.c:209), visible through
-- pg_backend_memory_contexts. Run via scripts/bugs-batch211-e2e.sh against
-- the frozen expected captured from C PostgreSQL 18.6.
\pset pager off
-- regression: one "Timezones" row at level 2 under TopMemoryContext.
SELECT name, ident, type, level, path[1] AS top
  FROM pg_backend_memory_contexts WHERE name = 'Timezones';
SET timezone = 'America/New_York';
SELECT '2026-01-01 00:00:00+00'::timestamptz;
SELECT count(*) FROM pg_backend_memory_contexts WHERE name = 'Timezones';

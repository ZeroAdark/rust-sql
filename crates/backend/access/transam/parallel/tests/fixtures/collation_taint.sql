\set VERBOSITY terse
CREATE DATABASE parallel_collation_taint TEMPLATE template0 LOCALE_PROVIDER icu ICU_LOCALE 'en-US' COLLATION_VERSION 'old';
\connect parallel_collation_taint
SET client_min_messages=warning;
SET debug_parallel_query=on;
SET max_parallel_workers_per_gather=1;
BEGIN;
ALTER DATABASE parallel_collation_taint REFRESH COLLATION VERSION;
SELECT pg_database_collation_actual_version(oid) IS NOT NULL FROM pg_database WHERE datname=current_database();
\! sleep 0.2
SELECT 42;
ROLLBACK;
\! sleep 0.2
SELECT 42;
\connect postgres
DROP DATABASE parallel_collation_taint;

-- bugs/batch-201-backend-utils-cache: cache fixes, expected captured from C 18.6.
\set VERBOSITY verbose
-- regression / rangetypes_selfuncs#1 / multirangetypes_selfuncs#1 /
-- network_selfuncs#1 / selfuncs-p3#1: examine_simple_variable (selfuncs.c:5733)
-- keeps the pg_statistic tuple pinned for the whole estimate, so a STABLE
-- operand that clears the column's statistics mid-planning still plans.
CREATE TABLE b201_stats (a integer, arr integer[], r int4range, ip inet, mr int4multirange);
INSERT INTO b201_stats
  SELECT i, ARRAY[i, i + 1], int4range(i, i + 10), ('10.0.0.' || (i % 250))::inet,
         int4multirange(int4range(i, i + 2))
  FROM generate_series(1, 1000) i;
ANALYZE b201_stats;
CREATE FUNCTION b201_clear() RETURNS integer LANGUAGE plpgsql STABLE AS $$
BEGIN
  PERFORM pg_clear_attribute_stats('public', 'b201_stats', 'a', false);
  PERFORM pg_clear_attribute_stats('public', 'b201_stats', 'arr', false);
  PERFORM pg_clear_attribute_stats('public', 'b201_stats', 'r', false);
  PERFORM pg_clear_attribute_stats('public', 'b201_stats', 'ip', false);
  PERFORM pg_clear_attribute_stats('public', 'b201_stats', 'mr', false);
  RETURN 5;
END $$;
BEGIN;
SELECT count(*) FROM b201_stats WHERE arr @> ARRAY[b201_clear()];
ROLLBACK;
ANALYZE b201_stats;
BEGIN;
SELECT count(*) FROM b201_stats WHERE r && int4range(b201_clear(), 20);
ROLLBACK;
ANALYZE b201_stats;
BEGIN;
SELECT count(*) FROM b201_stats WHERE ip << ('10.0.0.0/' || (b201_clear() + 19))::inet;
ROLLBACK;
ANALYZE b201_stats;
BEGIN;
SELECT count(*) FROM b201_stats WHERE mr && int4multirange(int4range(b201_clear(), 20));
ROLLBACK;
ANALYZE b201_stats;
BEGIN;
SELECT count(*) FROM b201_stats WHERE a = b201_clear();
ROLLBACK;
DROP TABLE b201_stats;
DROP FUNCTION b201_clear();
-- regression: get_ts_config_oid (namespace.c:3180) walks the active
-- search path without creating the temporary namespace, so SET
-- default_text_search_config needs no TEMP privilege even with pg_temp first.
CREATE ROLE b201_notemp;
DO $$ BEGIN EXECUTE format('REVOKE TEMP ON DATABASE %I FROM public', current_database()); END $$;
SET ROLE b201_notemp;
SET search_path = pg_temp, public;
SET default_text_search_config = 'english';
SHOW default_text_search_config;
SET default_text_search_config = 'b201_nosuch';
CREATE TEMP TABLE b201_temp (a integer);
RESET ROLE;
RESET search_path;
DO $$ BEGIN EXECUTE format('GRANT TEMP ON DATABASE %I TO public', current_database()); END $$;
DROP ROLE b201_notemp;

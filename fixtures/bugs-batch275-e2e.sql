\set VERBOSITY verbose
-- bugs/batch-275-mixed-main0915: expected captured from C 18.6 through
-- scripts/bugs-batch275-e2e.sh --capture (auto_explain durations normalized).
-- regression: IndexCheckExclusion prepares the predicate afresh
-- (index.c:3257), so an immutable predicate function is folded once more for
-- the validation pass. Parallel index build is off: C's
-- plan_create_index_workers adds a planner-side evaluation pgrust has no
-- counterpart for.
SET max_parallel_maintenance_workers = 0;
CREATE FUNCTION b275_note(int) RETURNS bool IMMUTABLE LANGUAGE plpgsql
  AS $$ BEGIN RAISE NOTICE 'b275_note(%)', $1; RETURN true; END $$;
CREATE TABLE b275_ex(a int);
ALTER TABLE b275_ex ADD CONSTRAINT b275_exc EXCLUDE (a WITH =) WHERE (b275_note(1));
INSERT INTO b275_ex VALUES (1), (2);
ALTER TABLE b275_ex ADD CONSTRAINT b275_exd EXCLUDE (a WITH =) WHERE (b275_note(2));
CREATE INDEX b275_exi ON b275_ex (a) WHERE b275_note(3);
RESET max_parallel_maintenance_workers;
-- regression: get_proposed_default_constraint canonicalizes the negated
-- constraint (partition.c:389): (b AND fk(c)) OR b collapses to b, so the
-- default-partition scan never calls fk for the redundant arm.
CREATE FUNCTION b275_fk(int) RETURNS bool IMMUTABLE LANGUAGE plpgsql
  AS $$ BEGIN RAISE NOTICE 'b275_fk(%)', $1; RETURN $1 > 0; END $$;
CREATE TABLE b275_pk(a int, b int, c int)
  PARTITION BY LIST (((a > 0) AND (((b > 0) AND b275_fk(c)) OR (b > 0))));
CREATE TABLE b275_pk_def PARTITION OF b275_pk DEFAULT;
INSERT INTO b275_pk VALUES (1, 1, 1), (1, 1, 2);
CREATE TABLE b275_pk_false PARTITION OF b275_pk FOR VALUES IN (false);
SELECT tableoid::regclass, a, b, c FROM b275_pk ORDER BY 1, 2, 3, 4;
-- regression: the missing SUPPORT function signature renders
-- its internal argument through format_type_be (functioncmds.c:699).
SET quote_all_identifiers = on;
CREATE FUNCTION b275_sf(int) RETURNS int LANGUAGE sql SUPPORT b275_nosuch AS 'select 1';
CREATE FUNCTION b275_sf(int) RETURNS int LANGUAGE sql SUPPORT "public"."b275 no such" AS 'select 1';
RESET quote_all_identifiers;
CREATE FUNCTION b275_sf(int) RETURNS int LANGUAGE sql SUPPORT b275_nosuch AS 'select 1';
-- regression: show_hash_info prints collected hash statistics
-- whenever nbatch > 0 (explain.c:3422), independently of es->analyze;
-- auto_explain re-reads log_analyze at render time.
LOAD 'auto_explain';
SET auto_explain.log_min_duration = 0;
SET auto_explain.log_analyze = on;
SET auto_explain.log_timing = off;
SET enable_mergejoin = off;
SET enable_nestloop = off;
SET client_min_messages = log;
SELECT set_config('auto_explain.log_analyze', 'off', true)
  FROM generate_series(1, 2000) a JOIN generate_series(1, 2000) b ON a = b LIMIT 1;
RESET client_min_messages;
RESET auto_explain.log_min_duration;
DROP TABLE b275_pk, b275_ex;
DROP FUNCTION b275_note(int), b275_fk(int);

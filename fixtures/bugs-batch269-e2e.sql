\set VERBOSITY verbose
-- regression: a zero-argument C alias of the contrib
-- handler returns a non-NULL TsmRoutine when called directly.
CREATE EXTENSION tsm_system_time;
CREATE FUNCTION b269_zt() RETURNS tsm_handler AS '$libdir/tsm_system_time', 'tsm_system_time_handler' LANGUAGE C;
SELECT b269_zt() IS NULL AS handler_null;
CREATE EXTENSION tsm_system_rows;
CREATE FUNCTION b269_zr() RETURNS tsm_handler AS '$libdir/tsm_system_rows', 'tsm_system_rows_handler' LANGUAGE C;
SELECT b269_zr() IS NULL AS handler_null;
CREATE TABLE b269_st(i int);
INSERT INTO b269_st SELECT g FROM generate_series(1, 100) g;
-- regression: GetTsmRoutine goes through fmgr, so a
-- sampling query links the handler's library into the session's file_list.
\c
SELECT count(*) >= 0 AS sampled FROM b269_st TABLESAMPLE system_time(1);
SELECT module_name FROM pg_get_loaded_modules() ORDER BY 1;
\c
SELECT count(*) AS sampled FROM b269_st TABLESAMPLE system_rows(3);
SELECT module_name FROM pg_get_loaded_modules() ORDER BY 1;
-- regression: a substituted lexeme is strlen-bounded
-- (cstring_to_text), so an embedded NUL truncates the result.
CREATE EXTENSION unaccent;
SELECT octet_length(unaccent('é' || ''::"char"::bpchar::text || 'é')) AS substituted, octet_length(unaccent('e' || ''::"char"::bpchar::text || 'e')) AS untouched;
-- regression: every result relation is validated
-- before any RETURNING projection is compiled.
CREATE EXTENSION file_fdw;
CREATE ROLE b269_u;
CREATE SERVER b269_fs FOREIGN DATA WRAPPER file_fdw;
CREATE TABLE b269_pt(i int, v text) PARTITION BY RANGE (i);
CREATE TABLE b269_pt1 PARTITION OF b269_pt FOR VALUES FROM (0) TO (10);
CREATE FOREIGN TABLE b269_pt2 PARTITION OF b269_pt FOR VALUES FROM (10) TO (20) SERVER b269_fs OPTIONS (filename '/dev/null', format 'csv');
INSERT INTO b269_pt VALUES (1, 'a');
CREATE FUNCTION b269_denied(text) RETURNS text LANGUAGE plpgsql AS $$ BEGIN RETURN upper($1); END $$;
REVOKE EXECUTE ON FUNCTION b269_denied(text) FROM public;
GRANT ALL ON b269_pt, b269_pt1, b269_pt2 TO b269_u;
SET ROLE b269_u;
UPDATE b269_pt SET v = 'b' RETURNING b269_denied(v);
DELETE FROM b269_pt RETURNING b269_denied(v);
UPDATE b269_pt SET v = 'b' WHERE i < 10 RETURNING b269_denied(v);
RESET ROLE;
UPDATE b269_pt SET v = 'b' WHERE i < 10 RETURNING b269_denied(v);
DROP TABLE b269_pt;
DROP SERVER b269_fs;
DROP ROLE b269_u;
-- regression: output-column input functions are resolved
-- only after the relation is opened and checked, so a failed open loads no
-- library for a wrapper's column types.
CREATE EXTENSION pgrowlocks;
CREATE EXTENSION hstore;
CREATE FUNCTION b269_rl(text) RETURNS TABLE(locked_row tid, locker xid, multi bool, xids xid[], modes text[], pids int[], extra hstore)
  AS '$libdir/pgrowlocks', 'pgrowlocks' LANGUAGE C STRICT;
\c
SELECT * FROM b269_rl('b269_no_such_table');
SELECT module_name FROM pg_get_loaded_modules() ORDER BY 1;
-- regression: crosstab_hash creates its output
-- tuplestore after the categories query, with the work_mem in effect then.
CREATE EXTENSION tablefunc;
CREATE TABLE b269_ct AS SELECT g AS rowid, 'c' || (g % 3) AS cat, repeat('v', 200) || g AS val FROM generate_series(1, 20000) g;
SET work_mem = '64kB';
SET temp_file_limit = 0;
SELECT count(*) FROM crosstab('SELECT rowid, cat, val FROM b269_ct ORDER BY 1,2', 'SELECT set_config(''work_mem'', ''256MB'', true) UNION ALL SELECT DISTINCT cat FROM b269_ct ORDER BY 1') AS ct(rowid int, c0 text, c1 text, c2 text, c3 text);
SHOW work_mem;
RESET work_mem;
RESET temp_file_limit;
-- regression: hash keys are evaluated in the per-tuple
-- context, so allocating key expressions do not accumulate over the build
-- and probe rows.
CREATE TABLE b269_hk AS SELECT g AS i, 'k' || g AS t FROM generate_series(1, 2000) g;
SET max_parallel_workers_per_gather = 0;
SET enable_mergejoin = off;
SET enable_nestloop = off;
SELECT max((SELECT sum(total_bytes) FROM pg_backend_memory_contexts WHERE a.i > 0)) < 64 * 1024 * 1024 AS bounded, count(*)
  FROM b269_hk a JOIN b269_hk b ON repeat('x', 100000) || a.t = repeat('x', 100000) || b.t;
RESET max_parallel_workers_per_gather;
RESET enable_mergejoin;
RESET enable_nestloop;

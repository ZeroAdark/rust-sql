-- bugs/batch-105-backend-access-misc: C-vs-pgrust parity for the fixes in
-- access/{table,tablesample}. Expected file captured from C 18.6
-- (scripts/regress-diff.sh --capture).
\set VERBOSITY verbose

-- heapam_relation_needs_toast_table accumulates the maximum tuple size in
-- int32 (-fwrapv): 128 x varchar(8388607) wraps to a 40-byte estimate and
-- gets no TOAST table; a negative total (52 x varchar(10485760)) gets one
SELECT 'CREATE TABLE b105_toast_wrap (' || string_agg('c' || i || ' varchar(8388607)', ', ' ORDER BY i) || ')' FROM generate_series(1,128) AS g(i) \gexec
SELECT reltoastrelid <> 0 AS has_toast FROM pg_class WHERE oid = 'b105_toast_wrap'::regclass;
SELECT count(*) FROM pg_depend WHERE refobjid = 'b105_toast_wrap'::regclass;
SELECT 'CREATE TABLE b105_toast_wrap2 (' || string_agg('c' || i || ' varchar(8388607)', ', ' ORDER BY i) || ', z varchar(100))' FROM generate_series(1,128) AS g(i) \gexec
SELECT reltoastrelid <> 0 AS has_toast FROM pg_class WHERE oid = 'b105_toast_wrap2'::regclass;
SELECT 'CREATE TABLE b105_toast_neg (' || string_agg(format('c%s varchar(10485760)', i), ',') || ')' FROM generate_series(1,52) AS g(i) \gexec
SELECT reltoastrelid <> 0 AS has_toast FROM pg_class WHERE oid = 'b105_toast_neg'::regclass;
CREATE TABLE b105_toast_small (a varchar(20), b int);
SELECT reltoastrelid <> 0 AS has_toast FROM pg_class WHERE oid = 'b105_toast_small'::regclass;
CREATE TABLE b105_toast_text (a text);
SELECT reltoastrelid <> 0 AS has_toast FROM pg_class WHERE oid = 'b105_toast_text'::regclass;
DROP TABLE b105_toast_wrap, b105_toast_wrap2, b105_toast_neg, b105_toast_small, b105_toast_text;

-- GetTsmRoutine invokes the handler through fmgr: a LANGUAGE internal
-- alias of a builtin sampling handler works like the builtin
CREATE FUNCTION b105_system_alias(internal) RETURNS tsm_handler AS 'tsm_system_handler' LANGUAGE internal;
CREATE FUNCTION b105_bernoulli_alias(internal) RETURNS tsm_handler AS 'tsm_bernoulli_handler' LANGUAGE internal STRICT;
CREATE TABLE b105_sample(i int);
INSERT INTO b105_sample SELECT g FROM generate_series(1, 5) g;
SELECT * FROM b105_sample TABLESAMPLE b105_system_alias(100) ORDER BY i;
SELECT * FROM b105_sample TABLESAMPLE b105_bernoulli_alias(100) ORDER BY i;
SELECT * FROM b105_sample TABLESAMPLE b105_bernoulli_alias(0);
SELECT count(*) FROM b105_sample TABLESAMPLE b105_system_alias(50) REPEATABLE (7);
SELECT * FROM b105_sample TABLESAMPLE b105_bernoulli_alias(200);
SELECT * FROM b105_sample TABLESAMPLE b105_system_alias(-1);
DROP TABLE b105_sample;
DROP FUNCTION b105_system_alias, b105_bernoulli_alias;

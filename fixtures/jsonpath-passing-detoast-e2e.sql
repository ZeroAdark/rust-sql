-- SQL/JSON PASSING arguments of text/varchar type over a toasted column
-- (regression audit XADA). The expected output is hand-verified, not captured
-- from C: PostgreSQL 18.6 jsonpath_exec.c:3062-3067 reads the toast
-- pointer via VARDATA_ANY without PG_DETOAST_DATUM and answers 24 / 68.
-- Run via scripts/jsonpath-passing-detoast-e2e.sh.
\set VERBOSITY verbose
\pset pager off

CREATE TABLE xada_t (c1 text, c2 varchar);
ALTER TABLE xada_t ALTER c1 SET STORAGE EXTERNAL;
ALTER TABLE xada_t ALTER c2 SET STORAGE EXTERNAL;
INSERT INTO xada_t VALUES (repeat('x', 10000), repeat('y', 10000));
-- out-of-line (EXTERNAL) text and varchar
SELECT bit_length(JSON_VALUE('{}'::jsonb, '$x' PASSING c1 AS x)) FROM xada_t;
SELECT bit_length(JSON_VALUE('{}'::jsonb, '$x' PASSING c2 AS x)) FROM xada_t;
SELECT length(JSON_QUERY('{}'::jsonb, '$x' PASSING c1 AS x)::text),
       JSON_EXISTS('{}'::jsonb, '$x' PASSING c1 AS x) FROM xada_t;
SELECT JSON_VALUE('{}'::jsonb, '$x' PASSING c1 AS x) = c1,
       JSON_VALUE('{}'::jsonb, '$x' PASSING c2 AS x) = c2 FROM xada_t;
SELECT JSON_QUERY('{}'::jsonb, '$x.size()' PASSING c1 AS x),
       JSON_VALUE('{}'::jsonb, '$x.type()' PASSING c2 AS x) FROM xada_t;
-- inline compressed (EXTENDED, the default)
ALTER TABLE xada_t ALTER c1 SET STORAGE EXTENDED;
INSERT INTO xada_t VALUES (repeat('ab', 4000), NULL);
SELECT bit_length(JSON_VALUE('{}'::jsonb, '$x' PASSING c1 AS x)),
       JSON_VALUE('{}'::jsonb, '$x' PASSING c1 AS x) = c1 FROM xada_t ORDER BY 1;
-- short and plain inline values are unchanged
SELECT JSON_VALUE('{}'::jsonb, '$x' PASSING 'hello'::text AS x),
       JSON_QUERY('{}'::jsonb, '$x' PASSING 'hello'::varchar AS x),
       JSON_VALUE('{}'::jsonb, '$x' PASSING NULL::text AS x) IS NULL;
DROP TABLE xada_t;

\set VERBOSITY verbose
-- regression: a RETURNS record alias of pg_sequence_parameters
-- resolves its descriptor through ReturnSetInfo.expectedDesc.
CREATE SEQUENCE b267_seq;
CREATE FUNCTION b267_seqparams(oid) RETURNS record AS 'pg_sequence_parameters' LANGUAGE internal;
SELECT * FROM b267_seqparams('b267_seq'::regclass) AS (start_value bigint, minimum_value bigint, maximum_value bigint, increment bigint, cycle_option bool, cache_size bigint, data_type oid);
-- regression: a non-strict binding hands gin_numeric_cmp a NULL
-- pointer; C orders it as the leftmost sentinel.
CREATE EXTENSION btree_gin;
CREATE FUNCTION b267_gin_numeric_cmp(numeric, numeric) RETURNS int AS '$libdir/btree_gin', 'gin_numeric_cmp' LANGUAGE C;
SELECT b267_gin_numeric_cmp(NULL, 1), b267_gin_numeric_cmp(1, NULL), b267_gin_numeric_cmp(NULL, NULL), b267_gin_numeric_cmp(2, 1), b267_gin_numeric_cmp(1, 2);
-- regression: REFRESH CONCURRENTLY stops probing indexes at the
-- first usable unique one (datafill runs before a later index's predicate can
-- raise), and probes the predicate before the expression-column test.
CREATE TABLE b267_t(a int);
INSERT INTO b267_t VALUES (1), (2);
CREATE FUNCTION b267_f() RETURNS int LANGUAGE sql IMMUTABLE AS 'SELECT 0';
CREATE SEQUENCE b267_mvseq;
CREATE MATERIALIZED VIEW b267_mv AS SELECT a, nextval('b267_mvseq') AS n FROM b267_t;
CREATE UNIQUE INDEX b267_mv_u ON b267_mv(a);
CREATE UNIQUE INDEX b267_mv_p ON b267_mv(a) WHERE a > b267_f();
CREATE MATERIALIZED VIEW b267_mvb AS SELECT a FROM b267_t;
CREATE UNIQUE INDEX b267_mvb_e ON b267_mvb((a + 0)) WHERE a > b267_f();
CREATE OR REPLACE FUNCTION b267_f() RETURNS int LANGUAGE sql IMMUTABLE AS 'SELECT 1/0';
\c
\set VERBOSITY terse
SELECT last_value FROM b267_mvseq;
REFRESH MATERIALIZED VIEW CONCURRENTLY b267_mv;
SELECT last_value FROM b267_mvseq;
REFRESH MATERIALIZED VIEW CONCURRENTLY b267_mvb;
\set VERBOSITY verbose
-- regression: an embedded NUL reaching set_config through a binary
-- bytea->text cast ends the C string there.
CREATE CAST (bytea AS text) WITHOUT FUNCTION;
SELECT set_config('application_name', '\x610062'::bytea::text, false);
SHOW application_name;
SELECT set_config('search_path', '\x7075626c696300787878'::bytea::text, false);
SHOW search_path;

-- bugs/batch-183-mixed-xhigh: SQL-level regressions, expected captured from C 18.6
-- (scripts/mixed-b183-e2e.sh normalizes auto_explain durations before diffing).
\set VERBOSITY verbose
-- regression: gseg_picksplit sorts by center with C's pg_qsort, where
-- -0 and +0 are equal keys (total_cmp put every -0 first): leaf memberships
-- are visible through pageinspect. 262 rows = one leaf's capacity + 1, so
-- exactly one (deterministic) split happens and no later insert reaches
-- gistchoose's random tie-break.
CREATE EXTENSION seg;
CREATE EXTENSION pageinspect;
CREATE TABLE b183_sg(s seg);
INSERT INTO b183_sg SELECT CASE WHEN i % 2 = 0 THEN '0'::seg ELSE '-0'::seg END FROM generate_series(1, 262) i;
CREATE INDEX b183_sg_i ON b183_sg USING gist(s);
SELECT pg_relation_size('b183_sg_i') / 8192 AS pages;
SELECT itemoffset, ctid, keys FROM gist_page_items(get_raw_page('b183_sg_i', 1), 'b183_sg_i') WHERE itemoffset <= 4;
SELECT itemoffset, ctid, keys FROM gist_page_items(get_raw_page('b183_sg_i', 2), 'b183_sg_i') WHERE itemoffset <= 4;
SELECT count(*), md5(string_agg(ctid::text || keys, ',' ORDER BY itemoffset)) FROM gist_page_items(get_raw_page('b183_sg_i', 1), 'b183_sg_i');
SELECT count(*), md5(string_agg(ctid::text || keys, ',' ORDER BY itemoffset)) FROM gist_page_items(get_raw_page('b183_sg_i', 2), 'b183_sg_i');
DROP TABLE b183_sg;
-- regression: EXPLAIN (BUFFERS) without ANALYZE
-- still carries INSTRUMENT_BUFFERS, so with log_analyze the logged plan is
-- the analyzed form ("(never executed)").
CREATE TABLE b183_ae(a int);
INSERT INTO b183_ae SELECT generate_series(1, 10);
LOAD 'auto_explain';
SET auto_explain.log_min_duration = 0;
SET auto_explain.log_analyze = on;
SET auto_explain.log_timing = off;
SET client_min_messages = log;
EXPLAIN (BUFFERS, COSTS OFF) SELECT count(*) FROM b183_ae;
EXPLAIN (COSTS OFF) SELECT count(*) FROM b183_ae;
RESET client_min_messages;
RESET auto_explain.log_min_duration;
DROP TABLE b183_ae;
-- SQL_ASCII rows: message bytes go out as the server-encoding bytes.
CREATE DATABASE b183_sa ENCODING 'SQL_ASCII' TEMPLATE template0;
\c b183_sa
CREATE EXTENSION isn;
CREATE EXTENSION pgstattuple;
-- regression: string2ean's messages keep the input bytes.
SELECT convert_from('\x61ff', 'SQL_ASCII')::ean13;
SELECT convert_from('\xe9', 'SQL_ASCII')::isbn;
SELECT convert_from('\x393739303132333435363738ff', 'SQL_ASCII')::isbn;
-- regression / pgstattuple#3: textToQualifiedNameList keeps the bytes.
SELECT pgstattuple(convert_from(decode('ff2e622e632e64', 'hex'), 'SQL_ASCII'));
SELECT pg_relpages(convert_from(decode('ff2e622e632e64', 'hex'), 'SQL_ASCII'));
SELECT pgstatindex(convert_from(decode('412e622e632e64', 'hex'), 'SQL_ASCII'));
-- regression: trace_notify's DEBUG1 keeps the channel bytes.
SET client_min_messages = debug1;
SET trace_notify = on;
SELECT pg_notify(convert_from('\x61e9', 'SQL_ASCII'), '');
RESET trace_notify;
RESET client_min_messages;
\c postgres
DROP DATABASE b183_sa;

-- Two-binary differential corpus for the 2026-09-13 bug-inventory batch
-- batch-208-backend-access-brin (C 18.6 oracle vs pgrust). Each leg is one
-- row of the batch (scripts/access-brin-b208-e2e.sh).
\set VERBOSITY verbose
CREATE DATABASE b208e2e TEMPLATE template0 ENCODING 'UTF8';
\c b208e2e
\set VERBOSITY verbose
-- regression: brin_build_desc hands the opcInfo proc the index attribute's
-- type (brin.c:1611), so a varchar opclass with STORAGE text builds and scans.
CREATE OPERATOR CLASS b208_vc_minmax_ops FOR TYPE varchar USING brin AS
  OPERATOR 1 <(text,text), OPERATOR 2 <=(text,text), OPERATOR 3 =(text,text),
  OPERATOR 4 >=(text,text), OPERATOR 5 >(text,text),
  FUNCTION 1 brin_minmax_opcinfo(internal),
  FUNCTION 2 brin_minmax_add_value(internal, internal, internal, internal),
  FUNCTION 3 brin_minmax_consistent(internal, internal, internal),
  FUNCTION 4 brin_minmax_union(internal, internal, internal),
  STORAGE text;
CREATE TABLE b208_vc(a varchar(20));
INSERT INTO b208_vc SELECT 'v' || lpad(g::text, 5, '0') FROM generate_series(1, 3000) g;
CREATE INDEX b208_vc_idx ON b208_vc USING brin(a b208_vc_minmax_ops) WITH (pages_per_range = 2);
SELECT atttypid::regtype FROM pg_attribute WHERE attrelid = 'b208_vc_idx'::regclass AND attnum = 1;
SET enable_seqscan = off;
SELECT count(*) FROM b208_vc WHERE a BETWEEN 'v00100' AND 'v00250';
SELECT count(*) FROM b208_vc WHERE a = 'v02345';
INSERT INTO b208_vc VALUES ('v99999');
SELECT count(*) FROM b208_vc WHERE a > 'v02999';
RESET enable_seqscan;
-- regression: an inet inclusion opclass without STORAGE leaves a
-- cidr index attribute typed cidr; the union summary takes that type.
CREATE OPERATOR CLASS b208_inet_incl_ops FOR TYPE inet USING brin AS
  OPERATOR 3 &&(inet,inet), OPERATOR 7 >>=(inet,inet), OPERATOR 8 <<=(inet,inet), OPERATOR 18 =(inet,inet),
  FUNCTION 1 brin_inclusion_opcinfo(internal),
  FUNCTION 2 brin_inclusion_add_value(internal, internal, internal, internal),
  FUNCTION 3 brin_inclusion_consistent(internal, internal, internal),
  FUNCTION 4 brin_inclusion_union(internal, internal, internal),
  FUNCTION 11 inet_merge(inet, inet),
  FUNCTION 12 inet_same_family(inet, inet),
  FUNCTION 13 network_overlap(inet, inet);
CREATE TABLE b208_cidr(a cidr);
INSERT INTO b208_cidr SELECT ('10.' || (g/256) || '.' || (g%256) || '.0/24')::cidr FROM generate_series(1, 2000) g;
CREATE INDEX b208_cidr_idx ON b208_cidr USING brin(a b208_inet_incl_ops) WITH (pages_per_range = 2);
SELECT atttypid::regtype FROM pg_attribute WHERE attrelid = 'b208_cidr_idx'::regclass AND attnum = 1;
INSERT INTO b208_cidr VALUES ('10.9.9.0/24');
SELECT brin_desummarize_range('b208_cidr_idx', 0), brin_summarize_range('b208_cidr_idx', 0);
SELECT count(*) FROM b208_cidr;
-- regression / regression: an opclass missing support function 2
-- (or 3) fails exactly where index_getprocinfo fails in C (indexam.c:946).
CREATE OPERATOR CLASS b208_incomplete_ops FOR TYPE int4 USING brin AS
  OPERATOR 1 <, OPERATOR 2 <=, OPERATOR 3 =, OPERATOR 4 >=, OPERATOR 5 >,
  FUNCTION 1 brin_minmax_opcinfo(internal);
CREATE TABLE b208_inc(a int4);
CREATE INDEX b208_inc_empty ON b208_inc USING brin(a b208_incomplete_ops);
INSERT INTO b208_inc VALUES (1);
INSERT INTO b208_inc SELECT g FROM generate_series(1, 100) g;
CREATE INDEX b208_inc_idx ON b208_inc USING brin(a b208_incomplete_ops);
SET enable_seqscan = off;
SELECT count(*) FROM b208_inc WHERE a = 5;
SELECT count(*) FROM b208_inc WHERE a IS NULL;
RESET enable_seqscan;
DROP INDEX b208_inc_empty;
CREATE OPERATOR CLASS b208_noconsistent_ops FOR TYPE int4 USING brin AS
  OPERATOR 1 <, OPERATOR 2 <=, OPERATOR 3 =, OPERATOR 4 >=, OPERATOR 5 >,
  FUNCTION 1 brin_minmax_opcinfo(internal),
  FUNCTION 2 brin_minmax_add_value(internal, internal, internal, internal);
CREATE INDEX b208_inc_idx2 ON b208_inc USING brin(a b208_noconsistent_ops) WITH (pages_per_range = 1);
SET enable_seqscan = off;
SELECT count(*) FROM b208_inc WHERE a = 5;
RESET enable_seqscan;
SELECT brin_summarize_new_values('b208_inc_idx2');
SELECT brin_desummarize_range('b208_inc_idx2', 0);
SELECT brin_summarize_range('b208_inc_idx2', 0);
-- regression: the DEBUG1 "index is not valid" report carries SQLSTATE
-- 55000 (brin.c:1470 / 1566).
CREATE TABLE b208_inv(a int4);
INSERT INTO b208_inv SELECT g FROM generate_series(1, 10) g;
CREATE INDEX CONCURRENTLY b208_inv_idx ON b208_inv USING brin ((1/(a-5)));
SELECT indisvalid FROM pg_index WHERE indexrelid = 'b208_inv_idx'::regclass;
SET client_min_messages = debug1;
SELECT brin_summarize_new_values('b208_inv_idx');
SELECT brin_desummarize_range('b208_inv_idx', 0);
RESET client_min_messages;
-- regression: brin_minmax_multi_summary_out appends the
-- output function's bytes verbatim (a SQL_ASCII database with high bytes).
CREATE DATABASE b208sa TEMPLATE template0 ENCODING 'SQL_ASCII' LC_COLLATE 'C' LC_CTYPE 'C';
\c b208sa
\set VERBOSITY verbose
CREATE FUNCTION b208_tdist(text, text) RETURNS float8 LANGUAGE sql IMMUTABLE STRICT AS 'SELECT 1.0::float8';
CREATE OPERATOR CLASS b208_text_mm_ops FOR TYPE text USING brin AS
  OPERATOR 1 <, OPERATOR 2 <=, OPERATOR 3 =, OPERATOR 4 >=, OPERATOR 5 >,
  FUNCTION 1 brin_minmax_multi_opcinfo(internal),
  FUNCTION 2 brin_minmax_multi_add_value(internal, internal, internal, internal),
  FUNCTION 3 brin_minmax_multi_consistent(internal, internal, internal, int4),
  FUNCTION 4 brin_minmax_multi_union(internal, internal, internal),
  FUNCTION 5 brin_minmax_multi_options(internal),
  FUNCTION 11 b208_tdist(text, text);
CREATE TABLE b208_l1(a text);
INSERT INTO b208_l1 VALUES (convert_from('\xe9'::bytea, 'SQL_ASCII')), ('abc'), (convert_from('\x41e9e9'::bytea, 'SQL_ASCII'));
CREATE INDEX b208_l1_idx ON b208_l1 USING brin(a b208_text_mm_ops);
CREATE EXTENSION pageinspect;
SELECT itemoffset, octet_length(value::text), encode(convert_to(value::text, 'SQL_ASCII'), 'hex')
  FROM brin_page_items(get_raw_page('b208_l1_idx', 2), 'b208_l1_idx');

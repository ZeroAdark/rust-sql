-- bugs/batch-229-mixed-lowtail: SQL-level regressions, expected captured from C 18.6.
\set VERBOSITY verbose
\set SHOW_CONTEXT always
-- regression: the local-buffer lookup hash is a "dynahash" context
-- labelled "Local Buffer Lookup Table" (pg_backend_memory_contexts shows the label).
CREATE TEMP TABLE b229_tmp (a int);
INSERT INTO b229_tmp VALUES (1);
SELECT name, ident, type, level FROM pg_backend_memory_contexts WHERE name = 'Local Buffer Lookup Table';
-- regression: aclcheck_error_type reports an array type by its element type.
CREATE ROLE b229_u NOLOGIN;
SET ROLE b229_u;
ALTER TYPE pg_catalog._int4 RENAME TO b229_foo;
ALTER TYPE pg_catalog._int4 SET SCHEMA pg_catalog;
RESET ROLE;
-- regression: the nondeterministic-collation refusal names the
-- opclass from its current pg_opclass row.
CREATE COLLATION b229_nd (provider = icu, locale = 'und', deterministic = false);
CREATE TABLE b229_t11 (a text);
ALTER OPERATOR CLASS text_pattern_ops USING btree RENAME TO b229_tpo;
CREATE INDEX ON b229_t11 (a COLLATE b229_nd b229_tpo);
ALTER OPERATOR CLASS b229_tpo USING btree RENAME TO text_pattern_ops;
CREATE INDEX ON b229_t11 (a COLLATE b229_nd text_pattern_ops);
DROP TABLE b229_t11;
DROP COLLATION b229_nd;
-- regression: SEQUENCE NAME goes through makeRangeVarFromNameList.
CREATE TABLE b229_t13 (a int GENERATED ALWAYS AS IDENTITY (SEQUENCE NAME a.b.c.d));
-- regression: the FROM relation of a constraint trigger is
-- ACL-checked with get_relkind_objtype, so a sequence/index/matview is named as such.
GRANT CREATE ON SCHEMA public TO b229_u;
SET ROLE b229_u;
CREATE TABLE b229_t15 (a int);
CREATE FUNCTION b229_f15() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RETURN NEW; END $$;
CREATE SEQUENCE b229_s15;
CREATE CONSTRAINT TRIGGER b229_tg AFTER INSERT ON b229_t15 FROM b229_s15 FOR EACH ROW EXECUTE FUNCTION b229_f15();
CREATE INDEX b229_i15 ON b229_t15 (a);
CREATE CONSTRAINT TRIGGER b229_tgi AFTER INSERT ON b229_t15 FROM b229_i15 FOR EACH ROW EXECUTE FUNCTION b229_f15();
CREATE MATERIALIZED VIEW b229_m15 AS SELECT 1 AS a;
CREATE CONSTRAINT TRIGGER b229_tgm AFTER INSERT ON b229_t15 FROM b229_m15 FOR EACH ROW EXECUTE FUNCTION b229_f15();
RESET ROLE;
DROP MATERIALIZED VIEW b229_m15;
DROP TABLE b229_t15;
DROP SEQUENCE b229_s15;
DROP FUNCTION b229_f15();
REVOKE CREATE ON SCHEMA public FROM b229_u;
DROP ROLE b229_u;
-- regression: FORMAT's value is read before the redundant-option check.
CREATE TABLE b229_t21 (a int);
COPY b229_t21 FROM stdin (FORMAT csv, FORMAT);
\.
COPY b229_t21 FROM stdin (FORMAT csv, FORMAT csv);
\.
DROP TABLE b229_t21;
-- regression: notices raised by row triggers and default
-- expressions during COPY FROM carry the COPY context.
CREATE TABLE b229_t22 (a int, b int DEFAULT 0);
CREATE FUNCTION b229_f22() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE NOTICE 'row %', NEW.a; RETURN NEW; END $$;
CREATE TRIGGER b229_tg22 BEFORE INSERT ON b229_t22 FOR EACH ROW EXECUTE FUNCTION b229_f22();
COPY b229_t22 (a) FROM stdin;
1
2
\.
COPY b229_t22 (a) FROM stdin WITH (FORMAT csv);
3
\.
CREATE FUNCTION b229_d22() RETURNS int LANGUAGE plpgsql AS $$ BEGIN RAISE WARNING 'default'; RETURN 7; END $$;
CREATE TABLE b229_t22b (a int, b int DEFAULT b229_d22());
COPY b229_t22b (a) FROM stdin;
5
\.
SELECT * FROM b229_t22 ORDER BY a;
SELECT * FROM b229_t22b;
DROP TABLE b229_t22, b229_t22b;
DROP FUNCTION b229_f22(), b229_d22();

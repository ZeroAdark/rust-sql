-- bugs/batch-241-mixed-reaudit: SQL-level regressions, expected captured from C 18.6.
\set VERBOSITY verbose
-- regression: PASSING args copied into the TableFunc, so a
-- single-use CTE inlined in place bumps each Var once.
CREATE TABLE b241_t1 (j jsonb, x int);
INSERT INTO b241_t1 VALUES ('{"a": 1}', 7);
SELECT sub.a
FROM b241_t1,
     LATERAL (WITH c AS (SELECT jt.a
                         FROM JSON_TABLE(b241_t1.j, '$' PASSING b241_t1.x AS x
                                         COLUMNS (a int PATH '$.a')) jt)
              SELECT s.a FROM (SELECT * FROM c) s) sub;
DROP TABLE b241_t1;
-- regression: MIN/MAX subroot outer_params identified after planning.
CREATE TABLE b241_o(a int); INSERT INTO b241_o VALUES (1);
CREATE TABLE b241_u1(k int); INSERT INTO b241_u1 VALUES (1);
CREATE TABLE b241_u2(b int); INSERT INTO b241_u2 SELECT g FROM generate_series(1,100000) g;
CREATE INDEX ON b241_u2(b);
ANALYZE b241_o, b241_u1, b241_u2;
SELECT (SELECT max(x) FROM (SELECT b241_o.a AS x FROM b241_u1 WHERE b241_u1.k = 1 UNION ALL SELECT b FROM b241_u2) ss) FROM b241_o;
DROP TABLE b241_o, b241_u1, b241_u2;
-- regression / regression: btoidfastcmp compares the truncated Oid.
CREATE TABLE b241_oid (o oid, s text);
INSERT INTO b241_oid VALUES ('2147483649', '2147483650'), ('2147483651', '2147483648');
SELECT o FROM (SELECT o FROM b241_oid UNION ALL SELECT s::oid FROM b241_oid) u ORDER BY o;
SET enable_hashagg = off;
SELECT DISTINCT o FROM (SELECT o FROM b241_oid UNION ALL SELECT o::text::oid FROM b241_oid) u ORDER BY 1;
RESET enable_hashagg;
SELECT array_agg(o ORDER BY o) FROM (SELECT o FROM b241_oid UNION ALL SELECT s::oid FROM b241_oid) u;
SELECT array_agg(o ORDER BY o) FROM (VALUES ((-2)::int4::oid), ('4294967295'::oid)) v(o);
DROP TABLE b241_oid;
-- regression: a NULL %ROWTYPE record is instantiated only
-- when a query resolves one of its fields.
CREATE TABLE b241_tbl(a int);
CREATE FUNCTION b241_f(go bool) RETURNS int LANGUAGE plpgsql AS $$
DECLARE r b241_tbl%ROWTYPE;
BEGIN
  IF go THEN RETURN 2; END IF;
  IF false THEN RETURN r.a; END IF;
  RETURN 1;
END $$;
SELECT b241_f(false);
DROP TABLE b241_tbl;
SELECT b241_f(true);
SELECT b241_f(false);
DROP FUNCTION b241_f(bool);
CREATE TABLE b241_tbl(a int);
CREATE FUNCTION b241_g() RETURNS int LANGUAGE plpgsql AS $$
DECLARE r b241_tbl%ROWTYPE;
BEGIN
  RETURN r.a;
END $$;
DROP TABLE b241_tbl;
SELECT b241_g();
DROP FUNCTION b241_g();

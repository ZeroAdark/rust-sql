-- bugs/batch-242-mixed-reaudit: SQL-level regressions, expected captured from C 18.6.
\set VERBOSITY verbose
-- regression: the serial arm admits block N+1 before block N's
-- eager accounting (read-stream look-ahead), so the success cap S freezes S+1
-- eagerly scanned pages; runs first so no checkpoint lands between the INSERT
-- and the first VACUUM.
CREATE TABLE b242_vac (a int, b text);
INSERT INTO b242_vac SELECT g, repeat('x', 500) FROM generate_series(1, 150000) g;
VACUUM b242_vac;
SELECT txid_current() > 0;
SET vacuum_freeze_min_age = 0;
VACUUM (PARALLEL 0) b242_vac;
RESET vacuum_freeze_min_age;
SELECT relallfrozen, relallvisible, relpages FROM pg_class WHERE relname = 'b242_vac';
DROP TABLE b242_vac;
-- regression: variadic array element type is the coerced node's.
CREATE TABLE b242_t13 (a int, b text);
INSERT INTO b242_t13 VALUES (1, 'x');
CREATE FUNCTION b242_rcat(record[], VARIADIC record[]) RETURNS record[] LANGUAGE internal AS 'array_cat';
SELECT b242_rcat(ARRAY[ROW(1,'y')], b242_t13, b242_t13) FROM b242_t13;
DROP FUNCTION b242_rcat(record[], record[]);
DROP TABLE b242_t13;
-- regression: parameter errors precede the trigger-return check.
SET check_function_bodies = off;
CREATE FUNCTION b242_tf(a int, "$1" int) RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RETURN NULL; END $$;
SET check_function_bodies = on;
SELECT b242_tf(1, 2);
DROP FUNCTION b242_tf(int, int);
-- regression: portal-arm param fetch errors carry the SQL statement.
CREATE FUNCTION b242_fors(assign bool) RETURNS void LANGUAGE plpgsql AS $$
DECLARE r record; i int;
BEGIN
  IF assign THEN r := row(1); END IF;
  FOR i IN SELECT r.f1 LOOP END LOOP;
END $$;
SELECT b242_fors(true);
SELECT b242_fors(false);
CREATE FUNCTION b242_open(assign bool) RETURNS void LANGUAGE plpgsql AS $$
DECLARE r record; c CURSOR FOR SELECT r.f1; x record;
BEGIN
  IF assign THEN r := row(1); END IF;
  OPEN c; FETCH c INTO x; CLOSE c;
END $$;
SELECT b242_open(false);
CREATE FUNCTION b242_forc(assign bool) RETURNS void LANGUAGE plpgsql AS $$
DECLARE r record; c CURSOR FOR SELECT r.f1; x record;
BEGIN
  IF assign THEN r := row(1); END IF;
  FOR x IN c LOOP END LOOP;
END $$;
SELECT b242_forc(false);
DROP FUNCTION b242_fors(bool), b242_open(bool), b242_forc(bool);
-- regression: make_tuple_from_row rejects a member whose
-- current type differs from the row tupdesc column.
CREATE TYPE b242_mt AS (x int, y int);
CREATE FUNCTION b242_f19(OUT a int, OUT b record) LANGUAGE plpgsql AS $$
BEGIN a := 1; b := (1,2)::b242_mt; END $$;
SELECT b242_f19();
CREATE FUNCTION b242_g19(OUT a int, OUT b record) RETURNS SETOF record LANGUAGE plpgsql AS $$
BEGIN a := 1; b := (1,2)::b242_mt; RETURN NEXT; END $$;
SELECT * FROM b242_g19();
CREATE FUNCTION b242_f19ok(OUT a int, OUT b record) LANGUAGE plpgsql AS $$
BEGIN a := 1; b := row(1,2); END $$;
SELECT b242_f19ok();
CREATE FUNCTION b242_g19ok(OUT a int, OUT b record) RETURNS SETOF record LANGUAGE plpgsql AS $$
BEGIN a := 1; b := row(1,2); RETURN NEXT; a := 2; b := NULL; RETURN NEXT; END $$;
SELECT * FROM b242_g19ok();
DROP FUNCTION b242_f19(), b242_g19(), b242_f19ok(), b242_g19ok();
DROP TYPE b242_mt;
-- regression: regress.c byte-exact messages and encoding names.
CREATE FUNCTION b242_pglz_decompress(bytea, int, bool) RETURNS bytea AS '$libdir/regress', 'test_pglz_decompress' LANGUAGE C STRICT;
SELECT b242_pglz_decompress('\x00'::bytea, 2147483647, false);
SELECT b242_pglz_decompress('\x00'::bytea, 2147483643, false);
CREATE FUNCTION b242_b2t(bytea) RETURNS text AS '$libdir/regress', 'test_bytea_to_text' LANGUAGE C STRICT;
CREATE FUNCTION b242_enc(bytea, name, name, bool, validlen OUT int, result OUT bytea) AS '$libdir/regress', 'test_enc_conversion' LANGUAGE C STRICT;
SELECT * FROM b242_enc('abc'::bytea, b242_b2t('\x55544638' || decode(repeat('20', 58), 'hex') || '\xff')::name, 'UTF8', false);
SELECT * FROM b242_enc('abc'::bytea, b242_b2t('\xff')::name, 'UTF8', false);
CREATE TYPE b242_widget;
CREATE FUNCTION b242_widget_in(cstring) RETURNS b242_widget AS '$libdir/regress', 'widget_in' LANGUAGE C STRICT IMMUTABLE;
CREATE FUNCTION b242_widget_out(b242_widget) RETURNS cstring AS '$libdir/regress', 'widget_out' LANGUAGE C STRICT IMMUTABLE;
CREATE TYPE b242_widget (internallength = 24, input = b242_widget_in, output = b242_widget_out, alignment = double);
SELECT b242_b2t('\xe9')::b242_widget;
SELECT '(1,2'::b242_widget;
DROP TYPE b242_widget CASCADE;
DROP FUNCTION b242_pglz_decompress(bytea, int, bool), b242_b2t(bytea), b242_enc(bytea, name, name, bool);

-- bugs/batch-273-mixed-main0915 fixes (expected captured from C 18.6)
\set VERBOSITY verbose
-- regression: widget_out %g spells nonfinite values NaN/Infinity
CREATE TYPE b273_widget;
CREATE FUNCTION b273_widget_in(cstring) RETURNS b273_widget AS '$libdir/regress', 'widget_in' LANGUAGE C STRICT IMMUTABLE;
CREATE FUNCTION b273_widget_out(b273_widget) RETURNS cstring AS '$libdir/regress', 'widget_out' LANGUAGE C STRICT IMMUTABLE;
CREATE TYPE b273_widget (internallength = 24, input = b273_widget_in, output = b273_widget_out, alignment = double);
SELECT '(nan,inf,-inf)'::b273_widget, '(1e100,1e-10,0)'::b273_widget, '(-0,123456789,0.0001234)'::b273_widget;
-- regression: a RETURNS record alias resolves through the column definition list
CREATE FUNCTION b273_enc(bytea, name, name, bool) RETURNS record AS '$libdir/regress', 'test_enc_conversion' LANGUAGE C STRICT;
SELECT * FROM b273_enc('\x41c3a9'::bytea, 'UTF8', 'LATIN1', false) AS (validlen int, result bytea);
SELECT * FROM b273_enc('\x41c3'::bytea, 'UTF8', 'LATIN1', true) AS (validlen int, result bytea);
-- regression: a GiST sortsupport proc that returns NULL is an error, not a panic
CREATE FUNCTION b273_nullret(internal) RETURNS void LANGUAGE internal AS 'pg_stat_get_snapshot_timestamp';
ALTER OPERATOR FAMILY box_ops USING gist ADD FUNCTION 11 (box, box) b273_nullret(internal);
CREATE TABLE b273_tb AS SELECT box(point(i,i),point(i+1,i+1)) b FROM generate_series(1,100) i;
DO $$ BEGIN
  CREATE INDEX b273_ix ON b273_tb USING gist (b);
EXCEPTION WHEN OTHERS THEN
  RAISE NOTICE '% %', SQLSTATE, regexp_replace(SQLERRM, '\d+', 'N');
END $$;
SELECT 1 AS still_connected;
DROP TABLE b273_tb;
DROP FUNCTION b273_nullret(internal);

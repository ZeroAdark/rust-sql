-- bugs/batch-233-mixed-lowtail: SQL-level regressions, expected captured from C 18.6
-- (scripts/mixed-b233-e2e.sh --capture "$PGINSTALL/postgres").
\set VERBOSITY verbose
-- regression: transformAExprBetween copyObject's the repeated
-- operands and transforms each copy, so a WARNING-emitting operand warns once
-- per comparison (2x for BETWEEN, 4x for the SYMMETRIC forms).
SELECT CURRENT_TIMESTAMP(7) BETWEEN now() - interval '1 day' AND now() + interval '1 day' AS b1;
SELECT CURRENT_TIMESTAMP(7) NOT BETWEEN now() - interval '1 day' AND now() + interval '1 day' AS b2;
SELECT CURRENT_TIMESTAMP(7) BETWEEN SYMMETRIC now() + interval '1 day' AND now() - interval '1 day' AS b3;
SELECT CURRENT_TIMESTAMP(7) NOT BETWEEN SYMMETRIC now() + interval '1 day' AND now() - interval '1 day' AS b4;
SELECT now() BETWEEN SYMMETRIC CURRENT_TIMESTAMP(7) + interval '1 day' AND CURRENT_TIMESTAMP(8) - interval '1 day' AS b5;
SELECT 1 BETWEEN 0 AND 2 AS b6, 1 NOT BETWEEN SYMMETRIC 2 AND 0 AS b7;
-- regression: a '#' directive without a grammar production
-- is a plain syntax error at the offending token.
DO $$ #foo BEGIN END $$;
DO $$ #"foo" BEGIN END $$;
DO $$ # 12 BEGIN END $$;
DO $$ #option foo BEGIN END $$;
DO $$ #print_strict_params on #variable_conflict use_column BEGIN END $$;
-- regression: read_sql_construct at end of input (no ';')
-- ends the construct like a terminator and the grammar reports the EOF; only
-- ';' reaches the 'missing "LOOP or USING"' diagnostic.
DO $$ DECLARE r record; BEGIN FOR r IN EXECUTE 'select 1' END $$;
DO $$ DECLARE r record; BEGIN FOR r IN EXECUTE 'select 1' $$;
DO $$ DECLARE r record; BEGIN FOR r IN EXECUTE 'select 1' USING 1 $$;
DO $$ DECLARE r record; BEGIN FOR r IN SELECT 1 $$;
DO $$ DECLARE x int; BEGIN x := 1 $$;
DO $$ DECLARE x int; BEGIN x := (1 $$;
DO $$ DECLARE x int; BEGIN IF true $$;
DO $$ DECLARE r record; BEGIN FOR r IN EXECUTE 'select 1'; END LOOP; END $$;
DO $$ DECLARE r record; BEGIN FOR r IN EXECUTE 'select 1' USING 1; END LOOP; END $$;
DO $$ DECLARE x int; BEGIN RAISE NOTICE 'x' $$;

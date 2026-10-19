-- Differential fixture for the 2026-09-13 bug batch 75 (pl/plpgsql: GET
-- DIAGNOSTICS area check after the whole item list, compound references are
-- never named cursor arguments, FOR ... IN EXECUTE terminator wording, and
-- parse-error positions counted in the database encoding).
-- Run via scripts/bugs-batch75-plpgsql-e2e.sh against the frozen expected
-- captured from C PostgreSQL 18.6.
\set VERBOSITY verbose
\pset pager off

-- regression: a later target error beats an earlier forbidden CURRENT item
DO $$ DECLARE a text; b CONSTANT integer := 0; BEGIN GET CURRENT DIAGNOSTICS a = MESSAGE_TEXT, b = ROW_COUNT; END $$;
DO $$ DECLARE a text; BEGIN GET CURRENT DIAGNOSTICS a = MESSAGE_TEXT a = ROW_COUNT; END $$;
DO $$ DECLARE a text; BEGIN GET CURRENT DIAGNOSTICS a = MESSAGE_TEXT, a = ROW_COUNT; END $$;
DO $$ DECLARE a text; b CONSTANT integer := 0; BEGIN GET STACKED DIAGNOSTICS a = ROW_COUNT, b = MESSAGE_TEXT; END $$;

-- regression / regression: r.x := / => is not named notation
DO $$DECLARE r record; c CURSOR("r.x" integer) FOR SELECT 1; BEGIN OPEN c(r.x => 1); CLOSE c; END$$;
DO $$DECLARE r record; c CURSOR("r.x" integer) FOR SELECT 1; BEGIN OPEN c(r.x := 1); CLOSE c; END$$;
DO $$DECLARE r record; c CURSOR(x integer) FOR SELECT 1; BEGIN OPEN c(r.x := 1); CLOSE c; END$$;
DO $$DECLARE c CURSOR(x integer) FOR SELECT 1; BEGIN OPEN c(y := 1); CLOSE c; END$$;
DO $$DECLARE c CURSOR(x integer, y integer) FOR SELECT x + y; v integer; BEGIN OPEN c(y := 1, x := 2); FETCH c INTO v; RAISE NOTICE '%', v; CLOSE c; END$$;
DO $$DECLARE r record; c CURSOR(x integer) FOR SELECT x; v integer; BEGIN SELECT 7 AS x INTO r; OPEN c(r.x); FETCH c INTO v; RAISE NOTICE '%', v; CLOSE c; END$$;

-- bug_5ce93953: FOR ... IN EXECUTE names both terminators
DO $$DECLARE r record; BEGIN FOR r IN EXECUTE 'select 1'; END$$;

-- regression: error position counted in the database encoding
DO $$BEGIN /*éé*/ END IF; END$$;
CREATE DATABASE b75ascii ENCODING 'SQL_ASCII' TEMPLATE template0;
\c b75ascii
\encoding SQL_ASCII
DO $$BEGIN /*éé*/ END IF; END$$;
DO $$BEGIN /*éé*/ x := 1; END$$;
\c postgres
DROP DATABASE b75ascii;

-- Differential fixture for the 2026-09-13 bug batch 74 (pl/plpgsql: block-
-- qualified %TYPE on a datatype-less record, stale cur_error after a throwing
-- handler, NULL-row assignment into a composite with a NOT NULL domain field,
-- OPEN ... NO FOR, duplicate INTO/USING cursor positions, COLLATE check
-- ordering, dynamic FOR target check before USING, FOREACH target check after
-- the body, GET DIAGNOSTICS target order, and three memory-growth probes:
-- rejected composite-domain field assignment, EXECUTE USING copies, and
-- composite-value record assignment).
-- Run via scripts/bugs-batch74-plpgsql-e2e.sh against the frozen expected
-- captured from C PostgreSQL 18.6.
\set VERBOSITY verbose
\pset pager off

-- regression: block.rec%TYPE on a datatype-less record falls through to type parsing
CREATE FUNCTION b74_qualified_new_type() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE x b74_qualified_new_type.new%TYPE; BEGIN RETURN NEW; END $$;
CREATE FUNCTION b74_qualified_var_type() RETURNS integer LANGUAGE plpgsql AS $$
DECLARE v integer := 5; w b74_qualified_var_type.v%TYPE := 6; BEGIN RETURN v + w; END $$;
SELECT b74_qualified_var_type();
DROP FUNCTION b74_qualified_var_type;

-- regression: cur_error is restored when an exception handler throws
CREATE FUNCTION b74_stale_error() RETURNS text LANGUAGE plpgsql AS $$
DECLARE msg text;
BEGIN
  BEGIN
    BEGIN RAISE EXCEPTION 'inner'; EXCEPTION WHEN OTHERS THEN RAISE EXCEPTION 'outer'; END;
  EXCEPTION WHEN OTHERS THEN NULL;
  END;
  GET STACKED DIAGNOSTICS msg = MESSAGE_TEXT;
  RETURN msg;
END $$;
SELECT b74_stale_error();
CREATE FUNCTION b74_nested_error() RETURNS text LANGUAGE plpgsql AS $$
DECLARE msg text;
BEGIN
  BEGIN
    RAISE EXCEPTION 'first';
  EXCEPTION WHEN OTHERS THEN
    BEGIN
      BEGIN RAISE EXCEPTION 'second'; EXCEPTION WHEN OTHERS THEN RAISE EXCEPTION 'third'; END;
    EXCEPTION WHEN OTHERS THEN NULL;
    END;
    GET STACKED DIAGNOSTICS msg = MESSAGE_TEXT;
  END;
  RETURN msg;
END $$;
SELECT b74_nested_error();
DROP FUNCTION b74_stale_error, b74_nested_error;

-- regression: a query returning no row makes the record all-NULL without field coercion
CREATE DOMAIN b74_nn AS integer NOT NULL;
CREATE TYPE b74_row AS (x b74_nn, y text);
DO $$ DECLARE r b74_row; BEGIN SELECT 1, 'a' INTO r WHERE false; RAISE NOTICE 'r=% x=% isnull=%', r, r.x, r IS NULL; END $$;
DO $$ DECLARE r b74_row; BEGIN FOR r IN SELECT 1, 'a' WHERE false LOOP NULL; END LOOP; RAISE NOTICE 'r=%', r; END $$;
DO $$ DECLARE r b74_row; BEGIN SELECT 1, 'a' INTO r WHERE false; r.x := NULL; END $$;
CREATE DOMAIN b74_rowdom AS b74_row CHECK ((VALUE).y IS NULL OR (VALUE).y <> 'bad');
DO $$ DECLARE r b74_rowdom; BEGIN SELECT 1, 'a' INTO r WHERE false; RAISE NOTICE 'r=% isnull=%', r, r IS NULL; END $$;
DROP DOMAIN b74_rowdom; DROP TYPE b74_row; DROP DOMAIN b74_nn;

-- regression: OPEN c NO FOR keeps the token after NO
DO $$ DECLARE c refcursor; r record; BEGIN OPEN c NO FOR SELECT 42 AS v; FETCH c INTO r; RAISE NOTICE 'v=%', r.v; CLOSE c; END $$;
DO $$ DECLARE c refcursor; BEGIN OPEN c NO SCROLL FOR SELECT 1; CLOSE c; END $$;
DO $$ DECLARE c refcursor; BEGIN OPEN c NO SELECT 1; CLOSE c; END $$;

-- regression: duplicate INTO / USING in EXECUTE point at the repeated clause
DO $$ DECLARE x integer; BEGIN EXECUTE 'SELECT $1' INTO x USING 1 INTO x; END $$;
DO $$ DECLARE x integer; BEGIN EXECUTE 'SELECT $1' USING 1 INTO x USING 2; END $$;
DO $$ DECLARE x integer; BEGIN EXECUTE 'SELECT $1' INTO x USING 1 INTO x, x; END $$;

-- regression: the COLLATE datatype check runs after the default expression is read
DO $$ DECLARE x integer COLLATE "C" DEFAULT ; BEGIN NULL; END $$;
DO $$ DECLARE x integer COLLATE "C"; BEGIN NULL; END $$;
DO $$ DECLARE x integer COLLATE nope DEFAULT ; BEGIN NULL; END $$;
DO $$ DECLARE x text COLLATE "C" NOT NULL DEFAULT 'a'; BEGIN RAISE NOTICE '%', x < 'B'; END $$;

-- regression: a dynamic FOR validates its target before the USING list
DO $$ BEGIN FOR x IN EXECUTE 'SELECT 1' USING LOOP NULL; END LOOP; END $$;
DO $$ DECLARE x int; BEGIN FOR x IN EXECUTE 'SELECT 1' USING LOOP NULL; END LOOP; END $$;
DO $$ DECLARE x int; BEGIN FOR x IN EXECUTE 'SELECT $1 + $2' USING 1, 2 LOOP RAISE NOTICE 'x=%', x; END LOOP; END $$;

-- regression: FOREACH checks its target after parsing the body
DO $$ DECLARE x CONSTANT integer := 0; BEGIN FOREACH x IN ARRAY ARRAY[1] LOOP missing := 1; END LOOP; END $$;
DO $$ DECLARE x CONSTANT integer := 0; BEGIN FOREACH x IN ARRAY ARRAY[1] LOOP NULL; END LOOP; END $$;
DO $$ BEGIN FOREACH nope IN ARRAY ARRAY[1] LOOP missing := 1; END LOOP; END $$;
DO $$ BEGIN FOREACH nope IN ARRAY ARRAY[1] LOOP NULL; END LOOP; END $$;
DO $$ DECLARE x integer; BEGIN FOREACH x IN ARRAY ARRAY[1,2] LOOP RAISE NOTICE 'x=%', x; END LOOP; END $$;

-- regression: GET DIAGNOSTICS rejects a non-scalar target before the CONSTANT check
DO $$ DECLARE a CONSTANT integer[] := ARRAY[1]; BEGIN GET DIAGNOSTICS a[1] = ROW_COUNT; END $$;
DO $$ DECLARE a CONSTANT integer := 1; BEGIN GET DIAGNOSTICS a = ROW_COUNT; END $$;
DO $$ DECLARE r CONSTANT record := NULL; BEGIN GET DIAGNOSTICS r = ROW_COUNT; END $$;

-- regression / pl_exec-p3#4: a rejected composite-domain field assignment keeps no copy
CREATE TYPE b74_payload AS (x text);
CREATE DOMAIN b74_small AS b74_payload CHECK (length((VALUE).x) < 10);
DO $$ DECLARE r b74_small; m0 bigint; m1 bigint; BEGIN
  r.x := 'ok';
  SELECT sum(used_bytes) INTO m0 FROM pg_backend_memory_contexts;
  FOR i IN 1..50 LOOP
    BEGIN r.x := repeat('x', 1048576); EXCEPTION WHEN check_violation THEN NULL; END;
  END LOOP;
  SELECT sum(used_bytes) INTO m1 FROM pg_backend_memory_contexts;
  RAISE NOTICE 'domain reject under 16MB: %, r.x = %', (m1 - m0) < 16 * 1048576, r.x;
END $$;
DROP DOMAIN b74_small; DROP TYPE b74_payload;

-- regression / pl_exec-p4#1: EXECUTE USING copies are statement-lifetime
DO $$ DECLARE s text := repeat('x', 1048576); n int; m0 bigint; m1 bigint; BEGIN
  SELECT sum(used_bytes) INTO m0 FROM pg_backend_memory_contexts;
  FOR i IN 1..50 LOOP EXECUTE 'SELECT length($1)' INTO n USING s; END LOOP;
  SELECT sum(used_bytes) INTO m1 FROM pg_backend_memory_contexts;
  RAISE NOTICE 'using under 16MB: %, n = %', (m1 - m0) < 16 * 1048576, n;
END $$;
CREATE FUNCTION b74_rq(s text) RETURNS SETOF int LANGUAGE plpgsql AS $$
DECLARE m0 bigint; m1 bigint; BEGIN
  SELECT sum(used_bytes) INTO m0 FROM pg_backend_memory_contexts;
  FOR i IN 1..50 LOOP RETURN QUERY EXECUTE 'SELECT length($1)' USING s; END LOOP;
  SELECT sum(used_bytes) INTO m1 FROM pg_backend_memory_contexts;
  RAISE NOTICE 'return query using under 16MB: %', (m1 - m0) < 16 * 1048576;
END $$;
SELECT count(*), min(b74_rq), max(b74_rq) FROM b74_rq(repeat('y', 1048576));
DROP FUNCTION b74_rq;
DO $$ DECLARE c refcursor; s text := repeat('z', 1048576); n int; BEGIN
  OPEN c FOR EXECUTE 'SELECT length($1)' USING s; s := 'gone'; FETCH c INTO n; CLOSE c;
  RAISE NOTICE 'cursor param n = %', n;
END $$;

-- regression: a composite value assigned to a record leaves no image behind
DO $$ DECLARE r record; s text := repeat('x', 1048576); m0 bigint; m1 bigint; BEGIN
  SELECT sum(used_bytes) INTO m0 FROM pg_backend_memory_contexts;
  FOR i IN 1..50 LOOP r := ROW(s, i); END LOOP;
  SELECT sum(used_bytes) INTO m1 FROM pg_backend_memory_contexts;
  RAISE NOTICE 'row assign under 16MB: %, f2 = %, len = %', (m1 - m0) < 16 * 1048576, r.f2, length(r.f1);
END $$;

CREATE TABLE src AS SELECT g AS i FROM generate_series(1,5) g;
CREATE FUNCTION peek() RETURNS bigint LANGUAGE plpgsql VOLATILE AS $$
DECLARE n bigint; BEGIN
  EXECUTE format('SELECT count(*) FROM public.pg_temp_%s', (SELECT oid FROM pg_class WHERE relname = 'mv')) INTO n;
  RETURN n;
EXCEPTION WHEN undefined_table THEN RETURN -1; END $$;
CREATE MATERIALIZED VIEW mv AS SELECT peek() AS seen FROM src;
SELECT array_agg(seen ORDER BY seen) FROM mv;
REFRESH MATERIALIZED VIEW mv;
SELECT array_agg(seen ORDER BY seen) FROM mv;
REFRESH MATERIALIZED VIEW mv;
SELECT array_agg(seen ORDER BY seen) FROM mv;

-- One correlated SubPlan evaluation costs the same however many SubPlans
-- the query holds: 1600 subqueries over 50 rows and 50 over 1600 rows run
-- the same 80000 evaluations.
CREATE FUNCTION wio496_exec_ms(q text) RETURNS float8 LANGUAGE plpgsql AS $$
DECLARE r text; ms float8;
BEGIN
  FOR r IN EXECUTE 'EXPLAIN (ANALYZE, TIMING OFF, SUMMARY ON) ' || q LOOP
    IF r LIKE 'Execution Time:%' THEN ms := substring(r FROM '([0-9.]+) ms')::float8; END IF;
  END LOOP;
  RETURN ms;
END $$;
SELECT 'SELECT sum(' || string_agg(format('(SELECT t.a + %s)', g), ' + ') || ') FROM generate_series(1, 50) t(a)' AS wide FROM generate_series(1, 1600) g \gset
SELECT 'SELECT sum(' || string_agg(format('(SELECT t.a + %s)', g), ' + ') || ') FROM generate_series(1, 1600) t(a)' AS narrow FROM generate_series(1, 50) g \gset
:wide;
:narrow;
SELECT wio496_exec_ms(:'narrow') AS narrow_ms, wio496_exec_ms(:'wide') AS wide_ms \gset
SELECT :wide_ms < 3 * :narrow_ms + 100 AS evaluation_cost_independent_of_subplan_count;
DROP FUNCTION wio496_exec_ms(text);

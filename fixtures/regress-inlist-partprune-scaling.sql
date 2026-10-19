-- Partition pruning of an IN list builds its steps in time linear in the
-- list: 8x the values may cost about 8x the planning time, not 64x.
SET statement_timeout = '120s';
CREATE TABLE wio485 (a int) PARTITION BY RANGE (a);
CREATE TABLE wio485_0 PARTITION OF wio485 FOR VALUES FROM (0) TO (1000000);
CREATE TABLE wio485_1 PARTITION OF wio485 FOR VALUES FROM (1000000) TO (2000000);
CREATE TABLE wio485l (a int) PARTITION BY LIST (a);
CREATE TABLE wio485l_d PARTITION OF wio485l DEFAULT;
CREATE FUNCTION wio485_plan_ms(tbl text, n int) RETURNS float8 LANGUAGE plpgsql AS $$
DECLARE q text; r text; ms float8;
BEGIN
  SELECT format('EXPLAIN (COSTS OFF, SUMMARY ON) SELECT * FROM %I WHERE a IN (', tbl)
         || string_agg((g * 7)::text, ',') || ')' INTO q FROM generate_series(1, n) g;
  FOR r IN EXECUTE q LOOP
    IF r LIKE 'Planning Time:%' THEN ms := substring(r FROM '([0-9.]+) ms')::float8; END IF;
  END LOOP;
  RETURN ms;
END $$;
EXPLAIN (COSTS OFF) SELECT * FROM wio485 WHERE a IN (7, 14, 1000007);
EXPLAIN (COSTS OFF) SELECT * FROM wio485 WHERE a IN (7, 14, 21);
SELECT wio485_plan_ms('wio485', 4000) AS small_ms, wio485_plan_ms('wio485', 32000) AS big_ms \gset
SELECT :big_ms < 16 * :small_ms + 50 AS range_pruning_linear_in_list_length;
SELECT wio485_plan_ms('wio485l', 4000) AS small_ms, wio485_plan_ms('wio485l', 32000) AS big_ms \gset
SELECT :big_ms < 16 * :small_ms + 50 AS list_pruning_linear_in_list_length;
DROP FUNCTION wio485_plan_ms(text, int);
DROP TABLE wio485, wio485l;

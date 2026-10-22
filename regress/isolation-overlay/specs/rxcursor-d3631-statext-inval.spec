# rxcursor D3631: a relation's extended-statistics list (rd_statlist) is shared across sessions through the shared catalog
# cache, under the relation's relcache generation. CREATE STATISTICS + ANALYZE and DROP STATISTICS in session a must reach
# the next plan of sessions b and c, which read the list before a's change (c first, so b may take c's shared copy), as
# PostgreSQL's per-session rd_statvalid does. The estimate of `a = 1 AND b = 1` over two equal columns is 1 row without
# the dependencies statistic and 100 rows with it.
# Expected output is PostgreSQL 18.6's (the C oracle).
setup
{
  CREATE TABLE d3631s (a int NOT NULL, b int NOT NULL);
  INSERT INTO d3631s SELECT g % 100, g % 100 FROM generate_series(1, 10000) g;
  ANALYZE d3631s;
  CREATE FUNCTION d3631_est(q text) RETURNS int LANGUAGE plpgsql AS $$
    DECLARE j json;
    BEGIN
      EXECUTE 'EXPLAIN (FORMAT JSON) ' || q INTO j;
      RETURN (j -> 0 -> 'Plan' ->> 'Plan Rows')::int;
    END $$;
}
teardown { DROP TABLE d3631s; DROP FUNCTION d3631_est(text); }

session a
step a_create	{ CREATE STATISTICS d3631s_dep (dependencies) ON a, b FROM d3631s; ANALYZE d3631s; }
step a_drop	{ DROP STATISTICS d3631s_dep; }

session b
step b_est	{ SELECT d3631_est('SELECT * FROM d3631s WHERE a = 1 AND b = 1') AS b_rows; }

session c
step c_est	{ SELECT d3631_est('SELECT * FROM d3631s WHERE a = 1 AND b = 1') AS c_rows; }

permutation c_est b_est a_create b_est c_est a_drop b_est c_est
permutation c_est b_est a_create a_drop c_est b_est

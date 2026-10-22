# A row lock over one outer-first hash join whose BUILD side may raise (op 14 holds the build's error for the probe's
# first row). PG keeps the EvalPlanQual plan tree for the statement: while no recheck has set the join's
# hj_OuterNotEmpty, a recheck fetches the outer's test row first and builds only if it passes the outer's filter; once
# one set it, later rechecks build first whatever the outer gives (ExecReScanHashJoin keeps the flag). hb's rows of
# a_id 3 move below the outer filter while ha's row 3 turns the build filter into a division by zero.
# pdrop: no recheck had an outer row, so each recheck of a_id 3 drops its test row before it builds: no error.
# praise: b1's recheck (a quiet build) sets the flag, so b3's recheck builds first and raises.
# Expected output is the C oracle's (PostgreSQL 18.6).

setup
{
  CREATE TABLE ha (id int PRIMARY KEY, v int NOT NULL);
  INSERT INTO ha SELECT i, i FROM generate_series(1, 5) i;
  CREATE TABLE hb (id int PRIMARY KEY, a_id int NOT NULL, w int NOT NULL);
  INSERT INTO hb SELECT i, (i - 1) % 5 + 1, i * 10 FROM generate_series(1, 15) i;
  ANALYZE ha, hb;
}

teardown
{
  DROP TABLE ha, hb;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1d	{ UPDATE ha SET v = 999 WHERE id = 3; UPDATE hb SET w = -1 WHERE a_id = 3; }
step s1f	{ UPDATE hb SET w = w WHERE id = 1; UPDATE ha SET v = 999 WHERE id = 3; UPDATE hb SET w = -1 WHERE a_id = 3; }
step s1c	{ COMMIT; }

session s2
setup		{ SET enable_nestloop = off; SET enable_mergejoin = off; }
step s2b	{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s2e	{ EXPLAIN (COSTS OFF) SELECT a.id, b.id FROM ha a JOIN hb b ON b.a_id = a.id WHERE b.w > 0 AND 100 / (a.v - 999) < 1000 FOR UPDATE; }
step s2r	{ SELECT a.id, b.id FROM ha a JOIN hb b ON b.a_id = a.id WHERE b.w > 0 AND 100 / (a.v - 999) < 1000 FOR UPDATE; }
step s2c	{ COMMIT; }

# pdrop
permutation s2b s2e s1d s2r s1c s2c
# praise
permutation s2b s1f s2r s1c s2c

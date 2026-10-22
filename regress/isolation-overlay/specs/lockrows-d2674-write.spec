# lockrows D2674: PG sets hj_OuterNotEmpty whenever the outer side's row reaches an outer-first hash join (its first fetch
# over an empty table, or a row with non-NULL keys), not only when the join returns a row. A write keeps its EvalPlanQual
# tree for the statement and ExecReScanHashJoin keeps the flag, so the next recheck over an empty build never runs the
# outer side. DELETE FROM tt USING tb b WHERE tt.id = b.id AND tt.f = 1 AND ptick(b.n), the target hashed on the build
# side, the outer a REFERENCE-marked scan with a VOLATILE qual.
# Permutation 1: recheck 1 (id 1, now f = 0) fetches the outer's row (ptick) over an empty build; recheck 2 (id 2, now
# f = 0) builds first and stops: pcnt 101.
# Permutation 2: recheck 1 (id 1 moved to id 1001) reaches the join with a non-NULL key and matches nothing; recheck 2
# (id 2, now f = 0) builds first and stops: pcnt 101.
# Gravity's marked probe (lowerv4 rerun/epq/marks.rs `epq_probe`) records those points in a frame word the driver reads
# after each called recheck (Entry::rerun), so recheck 2 skips its call over the empty build.
setup
{
  CREATE TABLE tt (id int PRIMARY KEY, f int NOT NULL);
  INSERT INTO tt VALUES (1, 1), (2, 1), (3, 1);
  CREATE TABLE tb (id int PRIMARY KEY, n int NOT NULL);
  INSERT INTO tb SELECT g, g * 10 FROM generate_series(1, 100) g;
  CREATE SEQUENCE pcnt;
  CREATE FUNCTION ptick(int) RETURNS bool LANGUAGE plpgsql VOLATILE AS $$ BEGIN PERFORM nextval('pcnt'); RETURN $1 >= 0; END $$;
  ANALYZE tt, tb;
}

teardown
{
  DROP TABLE tt, tb;
  DROP FUNCTION ptick(int);
  DROP SEQUENCE pcnt;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1e	{ UPDATE tt SET f = 0 WHERE id = 1; UPDATE tt SET f = 0 WHERE id = 2; }
step s1m	{ UPDATE tt SET id = 1001 WHERE id = 1; UPDATE tt SET f = 0 WHERE id = 2; }
step s1c	{ COMMIT; }

session s2
setup		{ SET enable_nestloop = off; SET enable_mergejoin = off; }
step s2b	{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s2x	{ EXPLAIN (COSTS OFF) DELETE FROM tt USING tb b WHERE tt.id = b.id AND tt.f = 1 AND ptick(b.n); }
step s2d	{ DELETE FROM tt USING tb b WHERE tt.id = b.id AND tt.f = 1 AND ptick(b.n); }
step s2last	{ SELECT last_value, is_called FROM pcnt; SELECT * FROM tt ORDER BY id; }
step s2c	{ COMMIT; }

permutation s2b s2x s1e s2d s1c s2last s2c
permutation s2b s1m s2d s1c s2last s2c

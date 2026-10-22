# lockrows D2050 (the write's form): PG keeps a write's EvalPlanQual recheck tree for the statement (EvalPlanQualBegin)
# and ExecReScanHashJoin keeps hj_OuterNotEmpty, so from the second recheck on an outer-first hash join whose outer gave a
# row builds first and, over an empty build, never runs its outer side.
# DELETE / UPDATE .. USING / FROM tb b WHERE tt.id = b.id AND tt.f = 1 AND ptick(b.n), the target hashed on the build
# side, the outer a REFERENCE-marked scan with a VOLATILE qual: recheck 1 (id 1, still f = 1) runs ptick once, recheck 2
# (id 2, now f = 0) none, so pcnt is 101. Gravity's write recheck reruns the build, then calls the stopped pipeline; after
# a recheck of the statement that passed a row, an empty build of such a probe (its BuildSide proto's BUILDSIDE_EPQ_FIRST,
# lowerv4 rerun/epq/marks.rs `epq_first`) skips that call (driver Entry::rerun), so ptick is not run again.
# Permutation 3: PG resets hj_OuterNotEmpty after a non-empty build. Recheck 1 (id 1) passes; recheck 2 (id 2, still
# f = 1) builds first, and its outer's pflip is false (the second call for n = 20), so the flag is clear again; recheck 3
# (id 3, now f = 0) takes the outer-first path and runs pflip once over its empty build: pcnt 103, ids 2 and 3 kept.
setup
{
  CREATE TABLE tt (id int PRIMARY KEY, f int NOT NULL);
  INSERT INTO tt VALUES (1, 1), (2, 1), (3, 1);
  CREATE TABLE tb (id int PRIMARY KEY, n int NOT NULL);
  INSERT INTO tb SELECT g, g * 10 FROM generate_series(1, 100) g;
  CREATE SEQUENCE pcnt;
  CREATE FUNCTION ptick(int) RETURNS bool LANGUAGE plpgsql VOLATILE AS $$ BEGIN PERFORM nextval('pcnt'); RETURN $1 >= 0; END $$;
  CREATE SEQUENCE p20;
  CREATE FUNCTION pflip(int) RETURNS bool LANGUAGE plpgsql VOLATILE AS $$ BEGIN PERFORM nextval('pcnt'); IF $1 = 20 THEN RETURN nextval('p20') < 2; END IF; RETURN true; END $$;
  ANALYZE tt, tb;
}

teardown
{
  DROP TABLE tt, tb;
  DROP FUNCTION ptick(int);
  DROP FUNCTION pflip(int);
  DROP SEQUENCE pcnt;
  DROP SEQUENCE p20;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1u	{ UPDATE tt SET f = 1 WHERE id = 1; UPDATE tt SET f = 0 WHERE id = 2; }
step s1u3	{ UPDATE tt SET f = 1 WHERE id = 1; UPDATE tt SET f = 1 WHERE id = 2; UPDATE tt SET f = 0 WHERE id = 3; }
step s1c	{ COMMIT; }

session s2
setup		{ SET enable_nestloop = off; SET enable_mergejoin = off; }
step s2b	{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s2x	{ EXPLAIN (COSTS OFF) DELETE FROM tt USING tb b WHERE tt.id = b.id AND tt.f = 1 AND ptick(b.n); }
step s2d	{ DELETE FROM tt USING tb b WHERE tt.id = b.id AND tt.f = 1 AND ptick(b.n); }
step s2u	{ UPDATE tt SET f = tt.f + 10 FROM tb b WHERE tt.id = b.id AND tt.f = 1 AND ptick(b.n); }
step s2f	{ DELETE FROM tt USING tb b WHERE tt.id = b.id AND tt.f = 1 AND pflip(b.n); }
step s2last	{ SELECT last_value, is_called FROM pcnt; SELECT * FROM tt ORDER BY id; }
step s2c	{ COMMIT; }

permutation s2b s2x s1u s2d s1c s2last s2c
permutation s2b s1u s2u s1c s2last s2c
permutation s2b s1u3 s2f s1c s2last s2c

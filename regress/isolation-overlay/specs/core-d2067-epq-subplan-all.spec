# D2067 (core executor, EvalPlanQual): an uncorrelated non-hashed SubPlan (ALL over a Material) on the probe side of a
# hash join under FOR UPDATE, rechecked once per row another session updated. PostgreSQL 18.6's EvalPlanQualStart
# builds the recheck's own SubPlan planstates with eflags 0, so the Material passes through and the subquery runs again
# at every recheck evaluation, where the main drive's Material (REWIND) runs it once. A VOLATILE call in the subquery
# counts the runs (last_value). Expected output is PostgreSQL 18.6's.

setup
{
  CREATE TABLE ua (id int PRIMARY KEY, v int NOT NULL);
  INSERT INTO ua SELECT i, i FROM generate_series(1, 5) i;
  CREATE TABLE ub (id int PRIMARY KEY, a_id int NOT NULL, w int NOT NULL);
  INSERT INTO ub SELECT i, (i - 1) % 5 + 1, i * 10 FROM generate_series(1, 15) i;
  CREATE TABLE uc (x int NOT NULL);
  INSERT INTO uc VALUES (7), (8), (9);
  CREATE SEQUENCE ucnt;
  CREATE FUNCTION utick(int) RETURNS int LANGUAGE plpgsql VOLATILE AS $$ BEGIN PERFORM nextval('ucnt'); RETURN $1; END $$;
  ANALYZE ua, ub, uc;
}

teardown
{
  DROP TABLE ua, ub, uc;
  DROP FUNCTION utick(int);
  DROP SEQUENCE ucnt;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1u	{ UPDATE ub SET w = w WHERE id IN (1, 6, 11); UPDATE ua SET v = v WHERE id = 2; }
step s1c	{ COMMIT; }

session s2
setup		{ SET enable_nestloop = off; SET enable_mergejoin = off; SET join_collapse_limit = 1; }
step s2b	{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s2e	{ EXPLAIN (COSTS OFF) SELECT a.id, b.id FROM ua a JOIN ub b ON b.a_id = a.id WHERE b.w > ALL (SELECT utick(x) FROM uc) FOR UPDATE; }
step s2q	{ SELECT a.id, b.id FROM ua a JOIN ub b ON b.a_id = a.id WHERE b.w > ALL (SELECT utick(x) FROM uc) FOR UPDATE; }
step s2last	{ SELECT last_value, is_called FROM ucnt; }
step s2c	{ COMMIT; }

# the ALL SubPlan's Material on the probe side, rechecked for each of the 4 rows s1 updated
permutation s2b s2e s1u s2q s1c s2last s2c

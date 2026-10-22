# D3707 (nestfast): a row lock with no LIMIT above it whose filter runs an EXISTS SubPlan that locks rows itself. Under
# READ COMMITTED: a concurrent update of an outer row makes the outer lock wait, then recheck it (EvalPlanQual: the scan's
# quals and the EXISTS again over the newest version, the inner lock locking again); a concurrent update or delete of an
# inner row makes the inner lock wait, then recheck that row (a version that fails its quals, or a deleted one, is
# skipped, so the EXISTS answers false).

setup
{
  CREATE TABLE d07 (id int PRIMARY KEY, grp int NOT NULL);
  INSERT INTO d07 SELECT i, i % 3 FROM generate_series(1, 14) i;
}

teardown
{
  DROP TABLE d07;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1g	{ UPDATE d07 SET grp = 7 WHERE id = 2; }
step s1d	{ DELETE FROM d07 WHERE id = 5; }
step s1m	{ UPDATE d07 SET id = 30 WHERE id = 7; }
step s1k	{ COMMIT; }

session s2
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s2x	{ EXPLAIN (COSTS OFF) SELECT j.id, j.grp FROM d07 j WHERE j.id <= 10 AND EXISTS (SELECT 1 FROM d07 k WHERE k.id = j.id + 1 FOR SHARE) FOR UPDATE OF j; }
step s2l	{ SELECT j.id, j.grp FROM d07 j WHERE j.id <= 10 AND EXISTS (SELECT 1 FROM d07 k WHERE k.id = j.id + 1 FOR SHARE) FOR UPDATE OF j; }
step s2k	{ COMMIT; }

permutation s2x s2k s1k
permutation s1g s2l s1k s2k
permutation s1d s2l s1k s2k
permutation s1m s2l s1k s2k

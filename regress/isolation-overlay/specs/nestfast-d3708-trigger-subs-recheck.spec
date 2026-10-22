# D3708 (nestfast): an UPDATE of one result relation with a BEFORE UPDATE ROW trigger whose filter runs an EXISTS SubPlan
# that locks rows. Under READ COMMITTED a concurrent update of a target row makes the trigger's lock wait
# (GetTupleForTrigger), then recheck it (EvalPlanQual: the EXISTS again over the newest version, its LockRows locking
# again): the trigger fires over the newest version's new row; a row whose key left the EXISTS is skipped; a deleted
# row is skipped.

setup
{
  CREATE TABLE d08 (id int PRIMARY KEY, v int NOT NULL);
  INSERT INTO d08 SELECT i, i * 10 FROM generate_series(1, 6) i;
  CREATE TABLE d08k (id int PRIMARY KEY, state text NOT NULL);
  INSERT INTO d08k SELECT i, CASE WHEN i = 4 THEN 'done' ELSE 'ready' END FROM generate_series(1, 20) i;
  CREATE FUNCTION d08_bu() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN NEW.v := NEW.v + 1000; RETURN NEW; END $$;
  CREATE TRIGGER d08_bu BEFORE UPDATE ON d08 FOR EACH ROW EXECUTE FUNCTION d08_bu();
}

teardown
{
  DROP TABLE d08, d08k;
  DROP FUNCTION d08_bu();
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1v	{ UPDATE d08 SET v = 77 WHERE id = 2; }
step s1i	{ UPDATE d08 SET id = 24 WHERE id = 3; }
step s1d	{ DELETE FROM d08 WHERE id = 5; }
step s1k	{ COMMIT; }
step s1s	{ SELECT * FROM d08 ORDER BY id; }

session s2
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s2x	{ EXPLAIN (COSTS OFF) UPDATE d08 t SET v = t.v + 1 WHERE EXISTS (SELECT 1 FROM d08k k WHERE k.id = t.id AND k.state = 'ready' FOR UPDATE) RETURNING t.id, t.v; }
step s2u	{ UPDATE d08 t SET v = t.v + 1 WHERE EXISTS (SELECT 1 FROM d08k k WHERE k.id = t.id AND k.state = 'ready' FOR UPDATE) RETURNING t.id, t.v; }
step s2k	{ COMMIT; }

permutation s2x s2k s1k
permutation s1v s2u s1k s2k s1s
permutation s1i s2u s1k s2k s1s
permutation s1d s2u s1k s2k s1s

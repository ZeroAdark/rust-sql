# D3801: Rails' update_all over a LEFT OUTER join that reads the joined table, written as a self-join on an alias
# (`UPDATE pets __active_record_update_alias SET .. FROM pets LEFT OUTER JOIN toys .. WHERE .. AND pets.pet_id =
# __active_record_update_alias.pet_id`) over fresh tables: PostgreSQL 18.6's Nested Loop over a Hash Right Join (toys
# probing, pets hashed) and the target's index scan, against a concurrent change of a target row (READ COMMITTED). The
# write finds the version its snapshot read updated and rechecks the newest one under EvalPlanQual: the Hash Right Join
# reruns over pets' and toys' refetched rows (toys' none when the row came null-extended), the target's index scan
# hands its newest version while it still matches the key, so a row whose other columns changed is written over its
# newest version, one whose key moved is skipped, a deleted one is gone. The steps return no rows (the drained rows'
# order is free, DECISIONS 6b); the table after each shows what was written. The expected output is PostgreSQL 18.6's.
setup
{
  CREATE TABLE pets (pet_id serial PRIMARY KEY, name text, n int DEFAULT 0);
  CREATE TABLE toys (toy_id serial PRIMARY KEY, pet_id int, name text);
  INSERT INTO pets (name) SELECT 'p' || i FROM generate_series(1, 6) i;
  INSERT INTO toys (pet_id, name) VALUES (1, 'ball'), (1, 'bone'), (2, 'ball'), (3, 'rope'), (1, 'ball');
}
teardown { DROP TABLE toys, pets; }

session s1
setup		{ BEGIN; }
step s1n4	{ UPDATE pets SET n = n + 10 WHERE pet_id = 4; }
step s1n1	{ UPDATE pets SET n = n + 10 WHERE pet_id = 1; }
step s1key	{ UPDATE pets SET pet_id = 99 WHERE pet_id = 5; }
step s1del	{ DELETE FROM pets WHERE pet_id = 6; }
step s1toy	{ UPDATE toys SET name = 'bone' WHERE toy_id = 3; }
step s1c	{ COMMIT; }

session s2
step s2x	{ EXPLAIN (COSTS OFF) UPDATE pets __active_record_update_alias SET name = COALESCE(toys.name || toys.toy_id, 'none') FROM pets LEFT OUTER JOIN toys ON toys.pet_id = pets.pet_id WHERE (toys.name = 'ball' OR toys.name IS NULL) AND pets.pet_id = __active_record_update_alias.pet_id; }
step s2upd	{ UPDATE pets __active_record_update_alias SET name = COALESCE(toys.name || toys.toy_id, 'none') FROM pets LEFT OUTER JOIN toys ON toys.pet_id = pets.pet_id WHERE (toys.name = 'ball' OR toys.name IS NULL) AND pets.pet_id = __active_record_update_alias.pet_id; }
step s2del	{ DELETE FROM pets a USING pets LEFT OUTER JOIN toys ON toys.pet_id = pets.pet_id WHERE (toys.name = 'ball' OR toys.name IS NULL) AND pets.pet_id = a.pet_id; }
step s2show	{ SELECT * FROM pets ORDER BY pet_id; }

permutation s2x s1n4 s2upd s1c s2show
permutation s1n1 s2upd s1c s2show
permutation s1key s2upd s1c s2show
permutation s1del s2upd s1c s2show
permutation s1toy s2upd s1c s2show
permutation s1n4 s1key s2del s1c s2show

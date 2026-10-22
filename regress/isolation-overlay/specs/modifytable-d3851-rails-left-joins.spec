# D3851: Rails' update_all over an inner join that also LEFT JOINs, a self-join on an alias (`UPDATE treasures
# __active_record_update_alias SET .. FROM treasures INNER JOIN pets_treasures .. LEFT OUTER JOIN ships ON ships.id =
# treasures.ship_id WHERE .. AND treasures.id = __active_record_update_alias.id`, and the comments form with a LEFT
# self-join and a nest over posts) over fresh tables: PostgreSQL 18.6's Nested Loop Left Join over the target's nest,
# the LEFT-joined relation's index scan its inner, against a concurrent change of a target row (READ COMMITTED). The
# write finds the version its snapshot read updated and rechecks the newest one under EvalPlanQual: the LEFT nest's
# inner is the REFERENCE-marked row refetched at its junk ctid (the version the statement joined, a concurrent change
# of it unseen), or none where the row came null-extended, so the SET reads what PostgreSQL's recheck reads; a target
# row deleted is skipped. Rails' `where.missing(:ship)` (LEFT JOIN ships .. WHERE ships.id IS NULL) plans an ANTI nest
# there: its rows carry ships' ctid NULL, so the recheck finds no ships row and the newest version passes again. The expected output is PostgreSQL 18.6's.
setup
{
  CREATE TABLE ships (id bigserial PRIMARY KEY, name varchar);
  CREATE TABLE treasures (id bigserial PRIMARY KEY, name varchar, ship_id bigint);
  CREATE TABLE pets_treasures (id bigserial PRIMARY KEY, treasure_id bigint, pet_id bigint);
  INSERT INTO ships (name) VALUES ('Black Pearl'), ('Interceptor');
  INSERT INTO treasures (name, ship_id) VALUES ('diamond', 1), ('sapphire', NULL), ('ruby', 2), ('emerald', 1);
  INSERT INTO pets_treasures (treasure_id, pet_id) VALUES (1, 1), (2, 1), (3, 2), (4, 1), (4, 1);
  CREATE TABLE posts (id bigserial PRIMARY KEY, title varchar);
  CREATE TABLE comments (id bigserial PRIMARY KEY, post_id bigint NOT NULL, body text NOT NULL, parent_id bigint);
  INSERT INTO posts (title) SELECT 'post ' || i FROM generate_series(1, 4) i;
  INSERT INTO comments (post_id, body, parent_id) VALUES (1, 'c1', NULL), (1, 'c2', 1), (2, 'c3', NULL), (2, 'c4', 3), (3, 'c5', 9);
}
teardown { DROP TABLE comments, posts, pets_treasures, treasures, ships; }

session s1
setup		{ BEGIN; }
step s1t1	{ UPDATE treasures SET name = name || '!' WHERE id = 1; }
step s1t2	{ UPDATE treasures SET name = name || '!' WHERE id = 2; }
step s1ship	{ UPDATE ships SET name = 'Flying Dutchman' WHERE id = 1; }
step s1del	{ DELETE FROM treasures WHERE id = 4; }
step s1ship2	{ UPDATE treasures SET ship_id = 1 WHERE id = 2; }
step s1c2	{ UPDATE comments SET body = body || '!' WHERE id = 2; }
step s1c3	{ UPDATE comments SET body = body || '!' WHERE id = 1; }
step s1c	{ COMMIT; }

session s2
step s2x	{ EXPLAIN (COSTS OFF) UPDATE treasures __active_record_update_alias SET name = __active_record_update_alias.name || '/' || COALESCE(ships.name, 'none') FROM treasures INNER JOIN pets_treasures ON treasures.id = pets_treasures.treasure_id LEFT OUTER JOIN ships ON ships.id = treasures.ship_id WHERE pets_treasures.pet_id = 1 AND treasures.id = __active_record_update_alias.id; }
step s2cx	{ EXPLAIN (COSTS OFF) UPDATE comments __active_record_update_alias SET body = __active_record_update_alias.body || '/' || COALESCE(parent.body, posts.title) FROM comments INNER JOIN posts ON posts.id = comments.post_id LEFT OUTER JOIN comments parent ON parent.id = comments.parent_id LEFT OUTER JOIN comments parents_comments ON parents_comments.id = parent.parent_id WHERE parent.parent_id IS NULL AND comments.parent_id IS NOT NULL AND comments.id = __active_record_update_alias.id; }
step s2upd	{ UPDATE treasures __active_record_update_alias SET name = __active_record_update_alias.name || '/' || COALESCE(ships.name, 'none') FROM treasures INNER JOIN pets_treasures ON treasures.id = pets_treasures.treasure_id LEFT OUTER JOIN ships ON ships.id = treasures.ship_id WHERE pets_treasures.pet_id = 1 AND treasures.id = __active_record_update_alias.id; }
step s2com	{ UPDATE comments __active_record_update_alias SET body = __active_record_update_alias.body || '/' || COALESCE(parent.body, posts.title) FROM comments INNER JOIN posts ON posts.id = comments.post_id LEFT OUTER JOIN comments parent ON parent.id = comments.parent_id LEFT OUTER JOIN comments parents_comments ON parents_comments.id = parent.parent_id WHERE parent.parent_id IS NULL AND comments.parent_id IS NOT NULL AND comments.id = __active_record_update_alias.id; }
step s2miss	{ DELETE FROM treasures a USING treasures INNER JOIN pets_treasures ON treasures.id = pets_treasures.treasure_id LEFT OUTER JOIN ships ON ships.id = treasures.ship_id WHERE pets_treasures.pet_id = 1 AND ships.id IS NULL AND treasures.id = a.id; }
step s2show	{ SELECT * FROM treasures ORDER BY id; }
step s2cshow	{ SELECT * FROM comments ORDER BY id; }

permutation s2x s1t1 s2upd s1c s2show
permutation s1t2 s2upd s1c s2show
permutation s1ship s1t1 s2upd s1c s2show
permutation s1del s2upd s1c s2show
permutation s1t2 s2miss s1c s2show
permutation s1ship2 s2miss s1c s2show
permutation s2cx s1c2 s2com s1c s2cshow
permutation s1c3 s1c2 s2com s1c s2cshow

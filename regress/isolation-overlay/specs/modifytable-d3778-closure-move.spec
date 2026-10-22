# D3778: TypeORM's closure-table move (`DELETE FROM closure WHERE descendant IN (SELECT descendant FROM closure
# subdescendant WHERE ancestor = 2 ..) AND ancestor NOT IN (SELECT descendant FROM closure subancestor WHERE ancestor
# = 2 ..)`), PostgreSQL 18.6's Hash Join with the target's scan filtered by a hashed NOT IN, against a concurrent
# change of a closure row it deletes (READ COMMITTED). The DELETE finds the version its snapshot read updated and
# rechecks the newest one under EvalPlanQual: the NOT IN probes the set built again from the same plan under the
# statement's snapshot (the statement's set here), the join reruns over the subdescendant row refetched at its ctid. A
# row whose ancestor moved elsewhere outside the subtree is deleted over its newest version, one whose ancestor moved
# into the subtree or whose descendant moved out of it is skipped, a deleted one is gone. The expected output is
# PostgreSQL 18.6's.
setup
{
  CREATE TABLE renamed_category (cat_id serial PRIMARY KEY, name text NOT NULL);
  CREATE TABLE renamed_category_closure (cat_id_ancestor int NOT NULL REFERENCES renamed_category (cat_id), cat_id_descendant int NOT NULL REFERENCES renamed_category (cat_id), PRIMARY KEY (cat_id_ancestor, cat_id_descendant));
  INSERT INTO renamed_category (name) VALUES ('root'), ('a'), ('a1'), ('b'), ('a2'), ('a1x'), ('c'), ('d');
  INSERT INTO renamed_category_closure VALUES (1, 1), (1, 2), (1, 3), (1, 4), (1, 5), (1, 6), (1, 7), (2, 2), (2, 3), (2, 5), (2, 6), (3, 3), (3, 6), (4, 4), (4, 7), (5, 5), (6, 6), (7, 7);
}
teardown { DROP TABLE renamed_category_closure, renamed_category; }

session s1
setup		{ BEGIN; }
step s1out	{ UPDATE renamed_category_closure SET cat_id_ancestor = 4 WHERE cat_id_ancestor = 1 AND cat_id_descendant = 3; }
step s1in	{ UPDATE renamed_category_closure SET cat_id_ancestor = 3 WHERE cat_id_ancestor = 1 AND cat_id_descendant = 5; }
step s1des	{ UPDATE renamed_category_closure SET cat_id_descendant = 8 WHERE cat_id_ancestor = 1 AND cat_id_descendant = 2; }
step s1del	{ DELETE FROM renamed_category_closure WHERE cat_id_ancestor = 1 AND cat_id_descendant = 6; }
step s1c	{ COMMIT; }

session s2
step s2x	{ EXPLAIN (COSTS OFF) DELETE FROM "renamed_category_closure" WHERE ("cat_id_descendant") IN (SELECT "cat_id_descendant" FROM (SELECT "cat_id_descendant" FROM "renamed_category_closure" "subdescendant" WHERE "subdescendant"."cat_id_ancestor" = 2) "descendant") AND ("cat_id_ancestor") NOT IN (SELECT "cat_id_descendant" FROM (SELECT "cat_id_descendant" FROM "renamed_category_closure" "subancestor" WHERE "subancestor"."cat_id_ancestor" = 2) "ancestor"); }
step s2mv	{ DELETE FROM "renamed_category_closure" WHERE ("cat_id_descendant") IN (SELECT "cat_id_descendant" FROM (SELECT "cat_id_descendant" FROM "renamed_category_closure" "subdescendant" WHERE "subdescendant"."cat_id_ancestor" = 2) "descendant") AND ("cat_id_ancestor") NOT IN (SELECT "cat_id_descendant" FROM (SELECT "cat_id_descendant" FROM "renamed_category_closure" "subancestor" WHERE "subancestor"."cat_id_ancestor" = 2) "ancestor") RETURNING cat_id_ancestor, cat_id_descendant; }
step s2show	{ SELECT cat_id_ancestor, cat_id_descendant FROM renamed_category_closure ORDER BY 1, 2; }

permutation s2x s1out s2mv s1c s2show
permutation s1in s2mv s1c s2show
permutation s1des s2mv s1c s2show
permutation s1del s2mv s1c s2show

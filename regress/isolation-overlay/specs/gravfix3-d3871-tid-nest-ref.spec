# D3871: Hibernate ORM 7's UPDATE with a join (MutationQueriesAndNotFoundActionTest#testUpdate: `update comments set
# text=$1 from comments c1_0 join users u1_0 on u1_0.id=c1_0.user_id where u1_0.id=$2 and comments.ctid=c1_0.ctid`), the
# target joined to itself again by ctid. PostgreSQL 18.6's generic plan is a Nested Loop (Nested Loop (Seq Scan c1_0,
# Tid Scan comments), Index Scan users): the target a Tid Scan nest inner, users (a REFERENCE mark) a nest inner past it.
# Another session changes a target row or its user first (READ COMMITTED), so s2's UPDATE waits on it, then EvalPlanQual
# rechecks: it refetches c1_0's and u1_0's rows at their junk ctids (the versions the join read), hands the Tid Scan the
# target's newest version, whose TID must still be the one c1_0's row names (nodeTidscan.c TidRecheck), and reruns the
# joins' quals.
#  (a) s1 changes the comment's text: the newest version has another TID, so s2 skips that row and updates the others
#  (b) s1 changes the user's name (not the comment): s2 updates every comment of the user (no wait on the comment)
#  (c) s1 deletes the comment: s2 skips it
#  (d) s1 aborts (control)
# Each prints the table after s2's write. The expected output is PostgreSQL 18.6's.
setup
{
  CREATE TABLE d3871u (id bigint PRIMARY KEY, name varchar(255));
  CREATE TABLE d3871c (id bigint PRIMARY KEY, text varchar(255), user_id bigint REFERENCES d3871u);
  INSERT INTO d3871u VALUES (1, 'a'), (2, 'b'), (3, 'c');
  INSERT INTO d3871c VALUES (10, 't10', 1), (11, 't11', 1), (12, 't12', 1), (20, 't20', 2), (30, 't30', 3);
}
teardown { DROP TABLE d3871c, d3871u; }

session s1
setup		{ BEGIN; }
step s1text	{ UPDATE d3871c SET text = 's1' WHERE id = 11; }
step s1name	{ UPDATE d3871u SET name = 'a1' WHERE id = 1; }
step s1del	{ DELETE FROM d3871c WHERE id = 11; }
step s1c	{ COMMIT; }
step s1a	{ ROLLBACK; }

session s2
setup		{ BEGIN; SET plan_cache_mode = force_generic_plan; PREPARE upd (varchar, bigint) AS update d3871c set text=$1 from d3871c c1_0 join d3871u u1_0 on u1_0.id=c1_0.user_id where u1_0.id=$2 and d3871c.ctid=c1_0.ctid; }
step s2x	{ EXPLAIN (COSTS OFF) EXECUTE upd('x', 1); }
step s2u	{ EXECUTE upd('s2', 1); }
step s2show	{ SELECT * FROM d3871c ORDER BY id; }
step s2c	{ COMMIT; }
teardown	{ DEALLOCATE upd; }

permutation s2x s1text s2u s1c s2show s2c
permutation s1name s2u s1c s2show s2c
permutation s1del s2u s1c s2show s2c
permutation s1text s2u s1a s2show s2c

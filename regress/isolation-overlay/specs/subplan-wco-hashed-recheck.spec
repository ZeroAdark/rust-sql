# W6-D1112 row B (subplan): an UPDATE through a view WITH CHECK OPTION whose qual is an uncorrelated IN, so the scan is
# a nested-loop semi join over the category table (a REFERENCE row mark) and the check option is a hashed SubPlan the
# host feeds. s2 waits on the item row s1 changed; after s1 commits, EvalPlanQual rechecks the row's newest version
# through the join, and the check option runs after the recheck in the main executor state with the hash table built
# at its first probe under the statement's snapshot (never rebuilt): a category s1 added is not in it, so s2's move to
# it fails the check; a row s1 moved out of the view is skipped; a row s1 changed in another column is written and
# checked. With s2 first, s1 waits on s2; after a rollback of s1, s2 writes the old version.
# s2 and s4 open their transaction in their first step, so a session a permutation leaves out holds none.
# The RLS form (s4, a non-superuser under an UPDATE policy WITH CHECK (cat = ANY (SELECT ...))): PostgreSQL evaluates
# the policy's check before each update attempt, so the first one builds the set; after s1's commit the recheck redoes
# the update over the newest version and the check runs again over the same set: a row the new version moves outside
# it fails, and a category s1 added is never in it.
# The per-row form (wh_vc: a correlated count in the view's qual and its check option, a SubPlan per row on the scan
# side and one the host feeds at the check): the recheck re-evaluates the scan side's over the newest version (a row s1
# moved out is skipped) and the check's runs after it under the statement's snapshot (a category s1 added counts 0).

setup
{
 CREATE TABLE wh_cat (code text PRIMARY KEY);
 INSERT INTO wh_cat VALUES ('a'), ('b'), ('c');
 CREATE TABLE wh_item (id int PRIMARY KEY, cat text NOT NULL, qty int NOT NULL);
 INSERT INTO wh_item SELECT g, (ARRAY['a', 'b', 'c'])[1 + g % 3], g FROM generate_series(1, 30) g;
 ANALYZE wh_cat, wh_item;
 CREATE VIEW wh_v AS SELECT * FROM wh_item WHERE cat IN (SELECT code FROM wh_cat) WITH CHECK OPTION;
 CREATE VIEW wh_vc AS SELECT * FROM wh_item i WHERE (SELECT count(*) FROM wh_cat c WHERE c.code = i.cat) = 1 WITH CHECK OPTION;
 CREATE TABLE wh_rl (id int PRIMARY KEY, cat text NOT NULL, qty int NOT NULL);
 INSERT INTO wh_rl SELECT g, (ARRAY['a', 'b', 'c'])[1 + g % 3], g FROM generate_series(1, 30) g;
 ANALYZE wh_rl;
 CREATE ROLE subplan_iso_wh_u;
 GRANT SELECT, UPDATE ON wh_rl TO subplan_iso_wh_u;
 GRANT SELECT ON wh_cat TO subplan_iso_wh_u;
 ALTER TABLE wh_rl ENABLE ROW LEVEL SECURITY;
 CREATE POLICY wh_rl_sel ON wh_rl FOR SELECT USING (true);
 CREATE POLICY wh_rl_upd ON wh_rl FOR UPDATE USING (true) WITH CHECK (cat = ANY (SELECT code FROM wh_cat));
}

teardown
{
 DROP VIEW wh_v;
 DROP VIEW wh_vc;
 DROP TABLE wh_item;
 DROP TABLE wh_rl;
 DROP TABLE wh_cat;
 DROP ROLE subplan_iso_wh_u;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1q	{ UPDATE wh_item SET qty = qty + 100 WHERE id = 1; }
step s1z	{ INSERT INTO wh_cat VALUES ('z'); UPDATE wh_item SET qty = 0 WHERE id = 2; }
step s1out	{ UPDATE wh_item SET cat = 'q' WHERE id = 3; }
step s1c	{ COMMIT; }
step s1r	{ ROLLBACK; }
step s1rq	{ UPDATE wh_rl SET qty = qty + 100 WHERE id = 1; }
step s1rz	{ INSERT INTO wh_cat VALUES ('z'); UPDATE wh_rl SET qty = 0 WHERE id = 2; }

session s2
step s2b	{ BEGIN ISOLATION LEVEL READ COMMITTED; UPDATE wh_v SET cat = 'b' WHERE id = 1 RETURNING id, cat, qty; }
step s2z	{ BEGIN ISOLATION LEVEL READ COMMITTED; UPDATE wh_v SET cat = 'z' WHERE id = 2 RETURNING id, cat, qty; }
step s2out	{ BEGIN ISOLATION LEVEL READ COMMITTED; UPDATE wh_v SET qty = -1 WHERE id = 3 RETURNING id, cat, qty; }
step s2bad	{ BEGIN ISOLATION LEVEL READ COMMITTED; UPDATE wh_v SET cat = 'x' WHERE id = 1 RETURNING id, cat, qty; }
step s2cnt	{ BEGIN ISOLATION LEVEL READ COMMITTED; UPDATE wh_vc SET cat = 'b' WHERE id = 1 RETURNING id, cat, qty; }
step s2cntz	{ BEGIN ISOLATION LEVEL READ COMMITTED; UPDATE wh_vc SET cat = 'z' WHERE id = 2 RETURNING id, cat, qty; }
step s2cntout	{ BEGIN ISOLATION LEVEL READ COMMITTED; UPDATE wh_vc SET qty = -1 WHERE id = 3 RETURNING id, cat, qty; }
step s2c	{ COMMIT; }

session s4
step s4b	{ BEGIN ISOLATION LEVEL READ COMMITTED; SET LOCAL ROLE subplan_iso_wh_u; UPDATE wh_rl SET cat = 'b' WHERE id = 1 RETURNING id, cat, qty; }
step s4moved	{ BEGIN ISOLATION LEVEL READ COMMITTED; SET LOCAL ROLE subplan_iso_wh_u; UPDATE wh_rl SET cat = CASE WHEN qty > 50 THEN 'x' ELSE 'b' END WHERE id = 1 RETURNING id, cat, qty; }
step s4z	{ BEGIN ISOLATION LEVEL READ COMMITTED; SET LOCAL ROLE subplan_iso_wh_u; UPDATE wh_rl SET cat = CASE WHEN qty = 0 THEN 'z' ELSE 'a' END WHERE id = 2 RETURNING id, cat, qty; }
step s4c	{ COMMIT; }

session s3
step s3sel	{ SELECT id, cat, qty FROM wh_item WHERE id <= 4 ORDER BY id; }
step s3rl	{ SELECT id, cat, qty FROM wh_rl WHERE id <= 3 ORDER BY id; }

permutation s1q s2b s1c s2c s3sel
permutation s1z s2z s1c s2c s3sel
permutation s1out s2out s1c s2c s3sel
permutation s1q s2bad s1c s2c s3sel
permutation s2b s1q s2c s1c s3sel
permutation s1q s2b s1r s2c s3sel
permutation s1rq s4b s1c s4c s3rl
permutation s1rq s4moved s1c s4c s3rl
permutation s1rz s4z s1c s4c s3rl
permutation s1q s2cnt s1c s2c s3sel
permutation s1z s2cntz s1c s2c s3sel
permutation s1out s2cntout s1c s2c s3sel

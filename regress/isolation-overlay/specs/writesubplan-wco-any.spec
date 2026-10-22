# writesubplan (regress updatable_views#978, D1112 row B): a routed INSERT into a view WITH CHECK OPTION whose qual is a
# correlated, non-hashed IN over a whole-row value (r IN (SELECT s FROM sometable s WHERE r.a = s.a)), the target a
# list-partitioned table whose leaf has another column order and a dropped column; and a plain table's view whose qual
# is a correlated <> ALL. The check option's SubPlan reads sometable under the INSERT's snapshot (READ COMMITTED: the
# statement's; REPEATABLE READ: the transaction's), so a row another session added or removed counts only once it is
# committed before that snapshot:
#  (a) s1 adds the matching row, uncommitted: s2's INSERT fails the check; after s1 commits, the same INSERT passes
#  (b) s1 deletes the matching row, uncommitted: s2's INSERT passes; after s1 commits, it fails
#  (c) s2 in REPEATABLE READ takes its snapshot first: s1's committed row is never seen, the INSERT fails
#  (d) the <> ALL view: s1 adds a conflicting row; s2 passes before s1 commits, fails after
# Expected output is PostgreSQL 18.6's (the C oracle).

setup
{
 CREATE TABLE wcowrtest (a int) PARTITION BY LIST (a);
 CREATE TABLE wcowrtest1 PARTITION OF wcowrtest FOR VALUES IN (1);
 ALTER TABLE wcowrtest ADD b text;
 CREATE TABLE wcowrtest2 (b text, c int, a int);
 ALTER TABLE wcowrtest2 DROP c;
 ALTER TABLE wcowrtest ATTACH PARTITION wcowrtest2 FOR VALUES IN (2);
 CREATE TABLE sometable (a int, b text);
 INSERT INTO sometable VALUES (1, 'a'), (2, 'b');
 CREATE VIEW wcowrtest_v2 AS SELECT * FROM wcowrtest r WHERE r IN (SELECT s FROM sometable s WHERE r.a = s.a) WITH CHECK OPTION;
 CREATE TABLE wsa_t (id int PRIMARY KEY, a int, d int);
 CREATE TABLE wsa_r (a int, d int);
 INSERT INTO wsa_r SELECT g, g * 10 FROM generate_series(1, 6) g;
 CREATE VIEW wsa_all AS SELECT * FROM wsa_t k WHERE k.a <> ALL (SELECT r.a FROM wsa_r r WHERE r.d <= k.d) WITH CHECK OPTION;
}

teardown
{
 DROP VIEW wcowrtest_v2, wsa_all;
 DROP TABLE wcowrtest, sometable, wsa_t, wsa_r;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1add	{ INSERT INTO sometable VALUES (2, 'no such row in sometable'); }
step s1delb	{ DELETE FROM sometable WHERE b = 'b'; }
step s1addr	{ INSERT INTO wsa_r VALUES (9, 5); }
step s1c	{ COMMIT; }

session s2
step s2ins	{ INSERT INTO wcowrtest_v2 VALUES (2, 'no such row in sometable'); }
step s2insb	{ INSERT INTO wcowrtest_v2 VALUES (2, 'b'); }
step s2rr	{ BEGIN ISOLATION LEVEL REPEATABLE READ; SELECT count(*) FROM sometable; }
step s2c	{ COMMIT; }
step s2all	{ INSERT INTO wsa_all VALUES (1, 9, 50) RETURNING *; }
step s2all2	{ INSERT INTO wsa_all VALUES (2, 9, 50) RETURNING *; }

session s3
step s3sel	{ SELECT tableoid::regclass, * FROM wcowrtest ORDER BY a, b; }
step s3t	{ SELECT * FROM wsa_t ORDER BY id; }

permutation s1add s2ins s1c s2ins s3sel
permutation s1delb s2insb s1c s2insb s3sel
permutation s2rr s1add s1c s2ins s2c s3sel
permutation s1addr s2all s1c s2all2 s3t

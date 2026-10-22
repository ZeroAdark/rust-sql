# M8-WRITE V4R-501: RETURNING OLD's system columns under a concurrent change (PostgreSQL 18's ExecUpdate and
# ExecOnConflictUpdate: OLD is the version the command actually replaced, after an EvalPlanQual recheck the newer
# version it locked). The victim (s2) blocks on a row s1 changed; after s1 commits:
#  (a) an UPDATE rechecks and replaces s1's version: old.ctid is s1's new ctid, old.xmin s1's (one transaction before
#      s2's: age 1), old.v s1's value
#  (b) the recheck fails the qual: nothing is replaced, no row returned
#  (c) s1 deleted the row: nothing is replaced
#  (d) ON CONFLICT DO UPDATE locks after s1's update: OLD is s1's version
#  (e) no concurrent change: OLD is the original version
# Expected output is PostgreSQL 18.6's (the C oracle).

setup
{
 CREATE TABLE ro (k int PRIMARY KEY, v int);
 INSERT INTO ro SELECT g, g * 10 FROM generate_series(1, 200) g;
 ANALYZE ro;
}

teardown
{
 DROP TABLE ro;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1u	{ UPDATE ro SET v = v + 1 WHERE k = 1; }
step s1del	{ DELETE FROM ro WHERE k = 1; }
step s1c	{ COMMIT; }

session s2
step s2u	{ UPDATE ro SET v = v * 2 WHERE k = 1 RETURNING old.ctid, new.ctid, old.v, new.v, age(old.xmin) AS old_age, old.xmin = new.xmin AS same_xmin, old.xmax = pg_current_xact_id()::xid AS old_xmax_mine; }
step s2uq	{ UPDATE ro SET v = v * 2 WHERE k = 1 AND v = 10 RETURNING old.ctid, new.ctid, old.v, new.v; }
step s2oc	{ INSERT INTO ro VALUES (1, 0) ON CONFLICT (k) DO UPDATE SET v = ro.v + 1000 RETURNING old.ctid, new.ctid, old.v, new.v, age(old.xmin) AS old_age, old.xmax = pg_current_xact_id()::xid AS old_xmax_mine; }
step s2own	{ UPDATE ro SET v = v + 5 WHERE k = 2 RETURNING old.ctid, new.ctid, old.v, new.v, old.xmin = pg_current_xact_id()::xid AS old_xmin_mine, old.xmax = pg_current_xact_id()::xid AS old_xmax_mine; }
step s2chk	{ SELECT ctid, * FROM ro WHERE k <= 2 ORDER BY k; }

permutation s1u s2u s1c s2chk
permutation s1u s2uq s1c s2chk
permutation s1del s2u s1c s2chk
permutation s1u s2oc s1c s2chk
permutation s2own s1c s2chk

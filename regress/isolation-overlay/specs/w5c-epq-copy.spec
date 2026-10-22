# M8-WRITE W5-C (I44): EvalPlanQual under a write whose target is hashed on a join's build side while the rechecking
# pipeline itself scans a COPY-marked row (a view's subquery row, a function row). The victim (s2) blocks on a target
# row s1 changed; after s1 commits, the recheck reruns the target's producer at its newest version and the emit from
# its copy entry over the row it stored (PostgreSQL's ROW_MARK_COPY: the whole row as first read, never re-evaluated).
#  (a) the target row's value changed: the newest version is written, RETURNING the copied view row
#  (b) the view's base row changed too: the recheck joins the stored (old) view row, as PostgreSQL
#  (c) the target row's key changed: the recheck no longer joins, the row is skipped
#  (d) a function row (FROM f()), under (a)
# The same four with OFFSET 0 in the view and the function (cvo, cfno), so the planner keeps their subquery: the COPY row
# is the SubqueryScan's on the emit itself (returning#51 / #54's form); cv and cfn are pulled up into plain joins.
# Expected output is PostgreSQL 18.6's (the C oracle).

setup
{
 CREATE TABLE ct (f1 int, f2 int);
 CREATE TABLE cb (f1 int, f2 int, f3 text);
 INSERT INTO ct VALUES (2, 20), (4, 40), (6, 60);
 INSERT INTO cb SELECT g, g * 10, 'b' || g FROM generate_series(1, 2000) g;
 CREATE VIEW cv AS SELECT * FROM cb WHERE f1 % 2 = 0;
 CREATE FUNCTION cfn() RETURNS SETOF cb LANGUAGE sql STABLE ROWS 2000 AS $$ SELECT * FROM cb WHERE f1 <= 10 $$;
 CREATE VIEW cvo AS SELECT * FROM cb WHERE f1 % 2 = 0 OFFSET 0;
 CREATE FUNCTION cfno() RETURNS SETOF cb LANGUAGE sql STABLE ROWS 2000 AS $$ SELECT * FROM cb WHERE f1 <= 10 OFFSET 0 $$;
 ANALYZE ct, cb;
}

teardown
{
 DROP FUNCTION cfno();
 DROP VIEW cvo;
 DROP FUNCTION cfn();
 DROP VIEW cv;
 DROP TABLE ct, cb;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1v	{ UPDATE ct SET f2 = f2 + 1 WHERE f1 = 2; }
step s1base	{ UPDATE ct SET f2 = f2 + 1 WHERE f1 = 2; UPDATE cb SET f3 = 'changed' WHERE f1 = 2; }
step s1key	{ UPDATE ct SET f1 = 3 WHERE f1 = 2; }
step s1c	{ COMMIT; }

session s2
setup
{
 SET enable_mergejoin = off;
 SET enable_nestloop = off;
}
step s2cnt	{ SELECT count(*) FROM ct; SELECT count(*) FROM cb; }
step s2plan	{ EXPLAIN (costs off) UPDATE ct SET f2 = ct.f2 + cv.f2 FROM cv WHERE cv.f1 = ct.f1 RETURNING cv; }
step s2view	{ UPDATE ct SET f2 = ct.f2 + cv.f2 FROM cv WHERE cv.f1 = ct.f1 RETURNING cv; }
step s2fn	{ UPDATE ct SET f2 = ct.f2 + cfn.f2 FROM cfn() WHERE cfn.f1 = ct.f1 RETURNING cfn; }
step s2plano	{ EXPLAIN (costs off) UPDATE ct SET f2 = ct.f2 + cvo.f2 FROM cvo WHERE cvo.f1 = ct.f1 RETURNING cvo; }
step s2viewo	{ UPDATE ct SET f2 = ct.f2 + cvo.f2 FROM cvo WHERE cvo.f1 = ct.f1 RETURNING cvo; }
step s2fno	{ UPDATE ct SET f2 = ct.f2 + cfno.f2 FROM cfno() WHERE cfno.f1 = ct.f1 RETURNING cfno; }
step s2chk	{ SELECT * FROM ct ORDER BY f1; }

permutation s2cnt s2plan s1c
permutation s1v s2view s1c s2chk
permutation s1base s2view s1c s2chk
permutation s1key s2view s1c s2chk
permutation s1v s2fn s1c s2chk
permutation s2plano s1c
permutation s1v s2viewo s1c s2chk
permutation s1base s2viewo s1c s2chk
permutation s1key s2viewo s1c s2chk
permutation s1v s2fno s1c s2chk

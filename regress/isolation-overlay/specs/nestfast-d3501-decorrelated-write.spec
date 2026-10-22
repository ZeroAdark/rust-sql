# D3501: an UPDATE .. FROM / DELETE .. USING whose source subquery selects a correlated count(*). pgrust's planner
# decorrelates it into a Hash Left Join over a grouped 'gravity_decorrelated' subquery (a COPY-marked row) above the
# source probing the hashed target. PostgreSQL plans the SubPlan per row; its EvalPlanQual refetches the source row at
# its junk ctid and evaluates the count again over it, which the stored grouped row matches. s2's write waits on the rows
# s1 changed; after s1 commits each newest version is rechecked: a row s1 changed in another column is written over its
# newest version, a row whose key s1 moved off the source row and a row s1 deleted are skipped, and the DELETE's filter
# (t.b < the count) reads the newest b. Target row 2's source row has no group: its count is 0 (the LEFT join
# null-extended), before and after the recheck. With s1 rolled back, s2 writes the old versions.

setup
{
 CREATE TABLE dws_tgt (a int, b int, c int, d int);
 CREATE TABLE dws_src (a int, b int, c int, d int);
 CREATE TABLE dws_ref (ab int, cd int);
 INSERT INTO dws_tgt SELECT i, i % 4, i * 10, i % 3 FROM generate_series(1, 12) g(i);
 INSERT INTO dws_src SELECT i, i % 3, i % 9, i % 5 FROM generate_series(3, 15) g(i);
 INSERT INTO dws_src VALUES (2, 0, 99, 0), (100, 1, 1, 1);
 INSERT INTO dws_ref SELECT i % 16, i % 9 FROM generate_series(1, 300) g(i);
 ANALYZE dws_tgt, dws_src, dws_ref;
}

teardown
{
 DROP TABLE dws_tgt, dws_src, dws_ref;
}

session s1
step s1b { BEGIN; }
step s1c { UPDATE dws_tgt SET c = c + 1 WHERE a IN (2, 5); }
step s1k { UPDATE dws_tgt SET a = 50 WHERE a = 6; }
step s1d { DELETE FROM dws_tgt WHERE a = 7; }
step s1f { UPDATE dws_tgt SET b = 9 WHERE a = 5; }
step s1m { COMMIT; }
step s1a { ROLLBACK; }

session s2
step s2u { UPDATE dws_tgt t SET d = s.cnt FROM (SELECT *, (SELECT count(*) FROM dws_ref r WHERE r.ab = s.a AND r.cd = s.c) cnt FROM dws_src s) s WHERE t.a = s.a; }
step s2d { DELETE FROM dws_tgt t USING (SELECT *, (SELECT count(*) FROM dws_ref r WHERE r.ab = s.a AND r.cd = s.c) cnt FROM dws_src s) s WHERE t.a = s.a AND t.b < s.cnt; }

session s3
step s3sel { SELECT a, b, c, d FROM dws_tgt ORDER BY a; }

permutation s2u s3sel
permutation s2d s3sel
permutation s1b s1c s1k s1d s2u s1m s3sel
permutation s1b s1c s1k s1d s2u s1a s3sel
permutation s1b s1f s1d s2d s1m s3sel
permutation s1b s1c s2d s1m s3sel

# M8-WRITE W6: MERGE's NOT MATCHED [BY TARGET] actions under a concurrent change (PostgreSQL 18's ExecMerge: a MATCHED
# row whose target version is gone or no longer joins at the recheck falls through to the NOT MATCHED actions). The
# victim (s2) blocks on a target row s1 changed; after s1 commits:
#  (a) s1 deleted the matched target row: the recheck finds no target, the source row takes the NOT MATCHED INSERT
#  (b) s1 moved the target row's key away: the same, the moved row stays
#  (c) s1 updated the matched target row's value: the MATCHED UPDATE runs over the newest version
#  (d) s1 inserted the key s2 would insert (a unique index): s2's INSERT waits, then raises the unique violation
# Expected output is PostgreSQL 18.6's (the C oracle).

setup
{
 CREATE TABLE nt (k int PRIMARY KEY, v int, note text DEFAULT 'dflt');
 CREATE TABLE ns (k int, w int);
 INSERT INTO nt VALUES (1, 10), (2, 20), (3, 30);
 INSERT INTO ns VALUES (1, 100), (2, 200), (4, 400);
 ANALYZE nt, ns;
}

teardown
{
 DROP TABLE nt, ns;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1del	{ DELETE FROM nt WHERE k = 1; }
step s1key	{ UPDATE nt SET k = 11 WHERE k = 1; }
step s1v	{ UPDATE nt SET v = v + 1000 WHERE k = 1; }
step s1ins	{ INSERT INTO nt VALUES (4, 44, 's1'); }
step s1c	{ COMMIT; }

session s2
setup
{
 SET enable_mergejoin = off;
 SET enable_nestloop = off;
}
step s2cnt	{ SELECT count(*) FROM nt; SELECT count(*) FROM ns; }
step s2plan	{ EXPLAIN (costs off) MERGE INTO nt t USING ns s ON t.k = s.k WHEN MATCHED THEN UPDATE SET v = t.v + s.w WHEN NOT MATCHED THEN INSERT VALUES (s.k, s.w); }
step s2merge	{ MERGE INTO nt t USING ns s ON t.k = s.k WHEN MATCHED THEN UPDATE SET v = t.v + s.w WHEN NOT MATCHED THEN INSERT VALUES (s.k, s.w); }
step s2chk	{ SELECT * FROM nt ORDER BY k; }

permutation s2cnt s2plan
permutation s1del s2merge s1c s2chk
permutation s1key s2merge s1c s2chk
permutation s1v s2merge s1c s2chk
permutation s1ins s2merge s1c s2chk

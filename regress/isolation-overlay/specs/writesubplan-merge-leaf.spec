# writesubplan (regress updatable_views#961-963): a MERGE through a partitioned root (directly and through a plain view)
# whose one leaf (another column order, dropped columns, under a sub-partitioned parent) probes the hashed source (Hash
# Right Join), with a WHEN MATCHED UPDATE and a WHEN NOT MATCHED INSERT routed through the root, while another session
# updates or deletes the matched leaf row. s2 waits on the row; once s1 commits, EvalPlanQual rechecks the join over the
# new version:
#  (a) s1 changes a non-key column: still matched, the UPDATE applies over the new version
#  (b) s1 moves the row's key: the pair no longer joins, the source row takes the NOT MATCHED INSERT (through the root)
#  (c) s1 deletes the row: NOT MATCHED, inserted
#  (d) as (a) with s1 rolled back: the old version
#  (e) through the view: as (b)
# Expected output is PostgreSQL 18.6's (the C oracle).

setup
{
 CREATE TABLE wml_pt (a int, b int, v varchar) PARTITION BY RANGE (a, b);
 CREATE TABLE wml_pt1 (b int NOT NULL, v varchar, a int NOT NULL) PARTITION BY RANGE (b);
 CREATE TABLE wml_pt11 (LIKE wml_pt1);
 ALTER TABLE wml_pt11 DROP a;
 ALTER TABLE wml_pt11 ADD a int;
 ALTER TABLE wml_pt11 DROP a;
 ALTER TABLE wml_pt11 ADD a int NOT NULL;
 ALTER TABLE wml_pt1 ATTACH PARTITION wml_pt11 FOR VALUES FROM (2) TO (5);
 ALTER TABLE wml_pt ATTACH PARTITION wml_pt1 FOR VALUES FROM (1, 2) TO (1, 10);
 CREATE VIEW wml_ptv AS SELECT * FROM wml_pt;
 INSERT INTO wml_pt VALUES (1, 2, 'two'), (1, 4, 'four');
}

teardown
{
 DROP VIEW wml_ptv;
 DROP TABLE wml_pt;
}

session s1
setup		{ BEGIN ISOLATION LEVEL READ COMMITTED; }
step s1v	{ UPDATE wml_pt SET v = 'new' WHERE b = 2; }
step s1k	{ UPDATE wml_pt SET b = 3 WHERE b = 2; }
step s1del	{ DELETE FROM wml_pt WHERE b = 2; }
step s1c	{ COMMIT; }
step s1r	{ ROLLBACK; }

session s2
step s2m	{
  MERGE INTO wml_pt t USING (VALUES (1, 2), (1, 3)) AS s(a, b) ON t.a = s.a AND t.b = s.b
  WHEN MATCHED THEN UPDATE SET v = t.v || ' merged' WHEN NOT MATCHED THEN INSERT VALUES (s.a, s.b, 'inserted');
}
step s2mv	{
  MERGE INTO wml_ptv t USING (VALUES (1, 2), (1, 3)) AS s(a, b) ON t.a = s.a AND t.b = s.b
  WHEN MATCHED THEN UPDATE SET v = t.v || ' merged' WHEN NOT MATCHED THEN INSERT VALUES (s.a, s.b, 'inserted');
}

session s3
step s3sel	{ SELECT tableoid::regclass, * FROM wml_pt ORDER BY b, v; }

permutation s1v s2m s1c s3sel
permutation s1k s2m s1c s3sel
permutation s1del s2m s1c s3sel
permutation s1v s2m s1r s3sel
permutation s1k s2mv s1c s3sel
